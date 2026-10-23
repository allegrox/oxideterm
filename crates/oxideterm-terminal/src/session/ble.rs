const BLE_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// Discovery window for saved-profile opens, which start from a cold adapter.
const BLE_SAVED_PROFILE_SCAN_WINDOW: std::time::Duration = std::time::Duration::from_secs(5);
/// A dropped link does not always end the notification stream (the stream can
/// stay open on platforms whose cache still reports the peer), so the worker
/// polls the link itself and closes the session once the peer is gone.
const BLE_LINK_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
const BLE_ATT_HEADER_BYTES: usize = 3;

/// Configuration for a BLE GATT terminal session backed by btleplug.
///
/// The adapter index and peripheral identity come from the discovery form;
/// the service UUID identifies which GATT service carries the notify/write
/// channel used as the terminal byte stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BleSessionConfig {
    pub adapter_index: usize,
    pub device_id: oxideterm_ble::BlePeripheralId,
    pub service_uuid: uuid::Uuid,
    pub device_label: String,
    /// BLE carries the same raw byte stream as a serial console, so both
    /// transports share their send/display options.
    pub runtime_options: SerialRuntimeOptions,
    /// Preferred ATT MTU requested right after the link comes up; `None`
    /// keeps the platform-negotiated default.
    pub mtu: Option<u16>,
}

impl BleSessionConfig {
    pub fn endpoint_label(&self) -> String {
        self.device_label.clone()
    }
}

pub struct BleSession {
    audit: Option<oxideterm_audit::AuditContext>,
    recording_sink: Option<oxideterm_audit::RecordingSink>,
    recording_pending: Option<crate::recording_output::RecordingOutput>,
    connect_audit: Option<oxideterm_audit::AuditOperation>,
    connected: bool,
    config: BleSessionConfig,
    term: Arc<FairMutex<Term<LocalEventListener>>>,
    parser: Processor,
    event_rx: LocalEventReceiver,
    worker_rx: crate::backpressure::ByteBoundedReceiver<BleWorkerEvent>,
    pending_events: Vec<TerminalEvent>,
    resize: TerminalResize,
    lifecycle: TerminalLifecycle,
    runtime: Option<Runtime>,
    command_tx: tokio::sync::mpsc::Sender<BleCommand>,
    title: Option<String>,
    graphics_ingress: GraphicsIngress,
    graphics: TerminalGraphicsState,
    palette: TerminalPalette,
    graphics_alt_screen_active: bool,
    output_queue: VecDeque<crate::backpressure::ByteBoundedItem<BleWorkerEvent>>,
    magic_scan: MagicScanWindow,
    encoding: TerminalEncoding,
    output_decoder: TerminalOutputDecoder,
    output_processor: Option<TerminalOutputProcessor>,
    output_events_enabled: bool,
    trigger_stream: Option<oxideterm_terminal_triggers::TerminalTriggerStream>,
    input_encoder: TerminalInputEncoder,
    encoding_detector: EncodingMismatchDetector,
    modem_consumer: ModemConsumer,
    shell_integration: TerminalShellIntegration,
    serial_console_ingress: SerialConsoleIngress,
    line_ending_ingress: LineEndingIngress,
    runtime_options: SerialRuntimeOptions,
    hexdump_offset: u64,
    /// ATT MTU currently negotiated with the peripheral; updated from worker events.
    negotiated_mtu: Option<u16>,
}

#[derive(Debug)]
enum BleCommand {
    Data(Vec<u8>),
    Close,
}

#[derive(Debug)]
enum BleWorkerEvent {
    Connected,
    /// ATT MTU negotiated by the OS/device; may arrive late on platforms that
    /// update the value after `subscribe` returns (macOS CoreBluetooth).
    NegotiatedMtu(u16),
    Output(Vec<u8>),
    Failed(String),
    Closed,
}

impl BleSession {
    pub fn new(
        config: BleSessionConfig,
        adapter_guard: Option<oxideterm_ble::BleAdapterGuard>,
        audit: Option<oxideterm_audit::AuditContext>,
        cols: usize,
        rows: usize,
        graphics_options: GraphicsOptions,
        encoding: TerminalEncoding,
        scrollback_lines: usize,
    ) -> Self {
        let resize = TerminalResize::new(cols, rows, 0, 0);
        let size = TerminalSize {
            cols: resize.cols,
            rows: resize.rows,
            cell_width: resize.cell_width,
            cell_height: resize.cell_height,
        };
        let (listener, event_rx) = local_event_channel();
        let recording_activity = listener.activity_sender();
        let (worker_tx, worker_rx) = crate::backpressure::byte_bounded_channel_with_activity(
            crate::backpressure::TRANSPORT_OUTPUT_BACKLOG_BYTES,
            listener.activity_sender(),
        );
        let (command_tx, command_rx) = tokio::sync::mpsc::channel(256);
        let term_config = interactive_terminal_config(scrollback_lines);
        let term = Arc::new(FairMutex::new(Term::new(term_config, &size, listener)));
        let connect_audit = Some(oxideterm_audit::AuditOperation::in_context(
            audit.as_ref(),
            oxideterm_audit::AuditCategory::Connection,
            "ble_connect",
            None,
        ));
        let recording_sink = audit
            .as_ref()
            .map(oxideterm_audit::AuditContext::recording_sink);
        if let Some(sink) = &recording_sink {
            sink.set_wake_callback(Arc::new(move || recording_activity.notify()));
        }
        let recording_pending = recording_sink.as_ref().map(|sink| {
            let mut pending = crate::recording_output::RecordingOutput::new(sink.clone());
            pending.resize(resize.cols as u16, resize.rows as u16);
            pending
        });

        let runtime = Runtime::new().ok();
        if let Some(runtime) = runtime.as_ref() {
            let worker_config = config.clone();
            runtime.spawn(run_ble_worker(worker_config, adapter_guard, command_rx, worker_tx));
        } else {
            let _ = worker_tx.send_control(BleWorkerEvent::Failed(
                "failed to initialize BLE runtime".to_string(),
            ));
        }

        let runtime_options = config.runtime_options;
        Self {
            audit,
            recording_sink,
            recording_pending,
            connect_audit,
            connected: false,
            config,
            term,
            parser: Processor::new(),
            event_rx,
            worker_rx,
            pending_events: Vec::new(),
            resize,
            lifecycle: TerminalLifecycle::Running,
            runtime,
            command_tx,
            title: None,
            graphics_ingress: GraphicsIngress::new(graphics_options),
            graphics: TerminalGraphicsState::default(),
            palette: TerminalPalette::default(),
            graphics_alt_screen_active: false,
            output_queue: VecDeque::new(),
            magic_scan: MagicScanWindow::default(),
            encoding,
            output_decoder: TerminalOutputDecoder::new(encoding),
            output_processor: None,
            output_events_enabled: false,
            trigger_stream: None,
            input_encoder: TerminalInputEncoder::new(encoding),
            encoding_detector: EncodingMismatchDetector::new(encoding),
            modem_consumer: ModemConsumer::new(),
            shell_integration: TerminalShellIntegration::default(),
            serial_console_ingress: SerialConsoleIngress::default(),
            line_ending_ingress: LineEndingIngress::default(),
            runtime_options,
            hexdump_offset: 0,
            negotiated_mtu: None,
        }
    }

    fn title_text(&self) -> String {
        format!("BLE {}", self.config.endpoint_label())
    }

    fn audit_exit(&mut self, outcome: oxideterm_audit::AuditOutcome) {
        if let Some(operation) = self.connect_audit.take() {
            operation.finish(
                outcome,
                oxideterm_audit::AuditEvidence::Lifecycle,
                None,
                None,
            );
        } else if self.connected {
            if let Some(context) = &self.audit {
                context.observe(
                    oxideterm_audit::AuditCategory::Connection,
                    "ble_disconnect",
                    None,
                    outcome,
                    oxideterm_audit::AuditEvidence::Lifecycle,
                    oxideterm_audit::AuditAuthorization::NotRequired,
                );
            }
            self.connected = false;
        }
    }

    fn drain_worker_events_with_budget(
        &mut self,
        budget: TerminalDrainBudget,
    ) -> TerminalDrainReport {
        let started = Instant::now();
        let mut report = TerminalDrainReport::default();
        loop {
            if self
                .recording_pending
                .as_mut()
                .is_some_and(|pending| !pending.flush())
            {
                break;
            }

            if budget.time_exhausted(started)
                || report.drained_bytes >= budget.max_bytes
                || report.events_drained >= budget.max_events
            {
                report.budget_exhausted =
                    !self.output_queue.is_empty() || !self.worker_rx.is_empty();
                break;
            }

            let event = match self
                .output_queue
                .pop_front()
                .map(Ok)
                .unwrap_or_else(|| self.worker_rx.try_recv())
            {
                Ok(event) => event,
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    if self.lifecycle.is_running() {
                        if let Some(sink) = &self.recording_sink {
                            sink.interrupt();
                        }
                        self.audit_exit(oxideterm_audit::AuditOutcome::Interrupted);
                        self.lifecycle = TerminalLifecycle::Exited(None);
                        self.pending_events.push(TerminalEvent::ChildExited(None));
                        report.mark_changed();
                    }
                    break;
                }
            };
            if let BleWorkerEvent::Output(bytes) = event.value()
                && report.drained_bytes > 0
                && report.drained_bytes.saturating_add(bytes.len()) > budget.max_bytes
            {
                self.output_queue.push_back(event);
                report.budget_exhausted = true;
                break;
            }

            match event.into_inner() {
                BleWorkerEvent::Connected => {
                    self.connected = true;
                    if let Some(operation) = self.connect_audit.take() {
                        operation.finish(
                            oxideterm_audit::AuditOutcome::Succeeded,
                            oxideterm_audit::AuditEvidence::Protocol,
                            None,
                            None,
                        );
                    }
                    self.title = Some(self.title_text());
                    self.pending_events
                        .push(TerminalEvent::TitleChanged(self.title_text()));
                    report.events_drained += 1;
                    report.mark_changed();
                }
                BleWorkerEvent::NegotiatedMtu(mtu) => {
                    if self.negotiated_mtu != Some(mtu) {
                        self.negotiated_mtu = Some(mtu);
                        report.events_drained += 1;
                        report.mark_changed();
                    }
                }
                BleWorkerEvent::Output(bytes) => {
                    report.events_drained += 1;
                    let processing_started = budget.collect_performance_metrics.then(Instant::now);
                    self.feed_transport_output(&bytes);
                    report.record_data_chunk(
                        bytes.len(),
                        processing_started.map_or(Duration::ZERO, |started| started.elapsed()),
                    );
                    report.mark_changed();
                }
                BleWorkerEvent::Failed(error) => {
                    if let Some(sink) = &self.recording_sink {
                        sink.interrupt();
                    }
                    self.audit_exit(oxideterm_audit::AuditOutcome::Failed);
                    self.lifecycle = TerminalLifecycle::Exited(None);
                    self.feed_utf8_terminal_output(
                        format!("\r\nBLE connection failed: {error}\r\n").as_bytes(),
                    );
                    self.pending_events.push(TerminalEvent::ChildExited(None));
                    report.events_drained += 1;
                    report.mark_changed();
                    break;
                }
                BleWorkerEvent::Closed => {
                    if self.lifecycle.is_running() {
                        if let Some(sink) = &self.recording_sink {
                            sink.interrupt();
                        }
                        self.audit_exit(oxideterm_audit::AuditOutcome::Interrupted);
                        self.lifecycle = TerminalLifecycle::Exited(None);
                        self.pending_events.push(TerminalEvent::ChildExited(None));
                        report.mark_changed();
                    }
                    report.events_drained += 1;
                    break;
                }
            }
        }
        report.pending_bytes = self.worker_rx.pending_bytes();
        report.drain_duration = started.elapsed();
        report
    }

    fn feed_transport_output(&mut self, bytes: &[u8]) {
        let events = self.modem_consumer.process_server_output(bytes);
        self.handle_modem_consumer_events(events);
    }

    fn feed_plain_transport_output(&mut self, bytes: &[u8]) {
        self.feed_display_output(bytes, true);
    }

    fn feed_display_output(&mut self, bytes: &[u8], remote_output: bool) {
        // Preserve protocol bytes before optional plugin display transforms.
        let processed_output = self.process_terminal_output(bytes);
        let bytes = processed_output.as_ref();
        let terminal_bytes = self.prepare_display_output(bytes);
        if terminal_bytes.is_empty() {
            return;
        }
        for kind in self.magic_scan.scan(&terminal_bytes) {
            self.pending_events.push(TerminalEvent::MagicDetected(kind));
        }
        let mut term = self.term.lock();
        let size = TerminalSize {
            cols: self.resize.cols,
            rows: self.resize.rows,
            cell_width: self.resize.cell_width,
            cell_height: self.resize.cell_height,
        };
        let cursor = Cell::new(graphics_cursor_from_term(&term, size));
        let mut protocol_responses = Vec::new();
        self.graphics_ingress.advance_ordered(
            &terminal_bytes,
            |segment| match segment {
                TerminalGraphicsSegment::Terminal(terminal_bytes) => {
                    if let Some(hint) = self.encoding_detector.observe(&terminal_bytes) {
                        self.pending_events.push(TerminalEvent::EncodingHint(hint));
                    }
                    let decoded = self.output_decoder.decode_to_utf8_bytes(&terminal_bytes);
                    if let Some(stream) = self.trigger_stream.as_mut() {
                        stream.observe_bytes(decoded.as_ref(), |matched| {
                            self.pending_events
                                .push(TerminalEvent::TriggerMatched(matched));
                        });
                    }
                    let record_output = remote_output
                        && (self.output_events_enabled
                            || self
                                .recording_sink
                                .as_ref()
                                .is_some_and(oxideterm_audit::RecordingSink::is_enabled));
                    if record_output {
                        let (_, recordable) = self.shell_integration.advance_with_recording(
                            &mut self.parser,
                            &mut *term,
                            decoded.as_ref(),
                            |event| self.pending_events.push(event),
                        );
                        if !recordable.is_empty() {
                            if let Some(pending) = &mut self.recording_pending {
                                pending.output(&recordable);
                            }
                            if self.output_events_enabled {
                                self.pending_events.push(TerminalEvent::Output(recordable));
                            }
                        }
                    } else {
                        self.shell_integration.advance(
                            &mut self.parser,
                            &mut *term,
                            decoded.as_ref(),
                            |event| self.pending_events.push(event),
                        );
                    }
                    self.graphics
                        .clear_for_alt_screen_transition(&term, &mut self.graphics_alt_screen_active);
                    cursor.set(graphics_cursor_from_term(&term, size));
                }
                TerminalGraphicsSegment::Event(event) => {
                    if let Some(response) = self.graphics.handle_event(event) {
                        protocol_responses.push(response);
                    }
                }
            },
            || cursor.get(),
        );
        drop(term);
        for response in protocol_responses {
            let _ = self.write_protocol_bytes(&response);
        }
    }

    fn process_terminal_output<'a>(&self, bytes: &'a [u8]) -> std::borrow::Cow<'a, [u8]> {
        apply_terminal_output_processor(&self.output_processor, bytes)
    }

    fn prepare_display_output(&mut self, bytes: &[u8]) -> Vec<u8> {
        match self.runtime_options.display_mode {
            SerialDisplayMode::Text => {
                // Serial ingress pairs: LineEndingIngress normalizes CR/LF,
                // SerialConsoleIngress escapes unterminated OSC/APC boot
                // noise that would otherwise swallow subsequent bytes. BLE-UART
                // bridge devices produce the same class of console traffic.
                let normalized = self
                    .line_ending_ingress
                    .normalize(bytes, self.runtime_options.output_line_ending);
                self.serial_console_ingress.filter(&normalized)
            }
            SerialDisplayMode::Hex => {
                format_serial_hexdump(bytes, &mut self.hexdump_offset, false)
            }
            SerialDisplayMode::Mixed => {
                format_serial_hexdump(bytes, &mut self.hexdump_offset, true)
            }
        }
    }

    fn encode_user_input(&self, bytes: &[u8]) -> Result<Vec<u8>> {
        match self.runtime_options.send_mode {
            SerialSendMode::Text => Ok(encode_text_line_endings(
                bytes,
                self.runtime_options.line_ending,
            )),
            SerialSendMode::Hex => parse_serial_hex_input(bytes),
        }
    }

    fn feed_utf8_terminal_output(&mut self, bytes: &[u8]) {
        self.push_output_event(bytes);
        let mut term = self.term.lock();
        self.shell_integration
            .advance(&mut self.parser, &mut *term, bytes, |event| {
                self.pending_events.push(event);
            });
    }

    fn push_output_event(&mut self, bytes: &[u8]) {
        if self.output_events_enabled && !bytes.is_empty() {
            self.pending_events.push(TerminalEvent::Output(bytes.to_vec()));
        }
    }

    fn handle_alacritty_event(&mut self, event: AlacEvent) -> bool {
        match event {
            AlacEvent::Title(title) => {
                self.title = Some(title.clone());
                self.pending_events.push(TerminalEvent::TitleChanged(title));
                false
            }
            AlacEvent::ResetTitle => {
                self.title = Some(self.title_text());
                self.pending_events
                    .push(TerminalEvent::TitleChanged(self.title_text()));
                false
            }
            AlacEvent::Bell => {
                self.pending_events.push(TerminalEvent::Bell);
                false
            }
            AlacEvent::Wakeup | AlacEvent::MouseCursorDirty => {
                self.pending_events.push(TerminalEvent::Wakeup);
                true
            }
            AlacEvent::CursorBlinkingChange => {
                let blinking = self.term.lock().cursor_style().blinking;
                self.pending_events
                    .push(TerminalEvent::BlinkChanged(blinking));
                true
            }
            AlacEvent::PtyWrite(text) => {
                let _ = self.write_protocol_bytes(text.as_bytes());
                false
            }
            AlacEvent::ClipboardStore(_, text) => {
                self.pending_events
                    .push(TerminalEvent::ClipboardStore(text));
                false
            }
            AlacEvent::ClipboardLoad(_, formatter) => {
                self.pending_events
                    .push(TerminalEvent::ClipboardLoad(formatter));
                false
            }
            AlacEvent::ColorSchemeRequest => {
                let _ = self.write_protocol_bytes(self.palette.color_scheme_report().as_bytes());
                false
            }
            AlacEvent::ColorRequest(index, formatter) => {
                let color = crate::color_for_alacritty_request(
                    index,
                    &self.palette,
                    self.term.lock().colors(),
                );
                let _ = self.write_protocol_bytes(formatter(color).as_bytes());
                false
            }
            AlacEvent::TextAreaSizeRequest(_) => false,
            AlacEvent::ChildExit(_) | AlacEvent::Exit => false,
        }
    }

    fn send_command(&mut self, command: BleCommand) -> Result<()> {
        self.command_tx
            .try_send(command)
            .map_err(|error| anyhow::anyhow!(error.to_string()))
    }
}

impl Drop for BleSession {
    fn drop(&mut self) {
        // Dropping this backend cancels its private runtime and any pending recording.
        self.shutdown();
    }
}

impl BleSession {
    fn flush_modem_server_writes(&mut self) -> bool {
        let Some(transfer) = self.modem_consumer.active_transfer_input() else {
            return false;
        };
        let mut changed = false;
        while let Some(bytes) = transfer.take_server_write() {
            let byte_len = bytes.len();
            if self.write_protocol_bytes(&bytes).is_ok() {
                transfer.complete_server_write(byte_len);
                changed = true;
            } else {
                transfer.restore_server_write(bytes);
                break;
            }
        }
        changed
    }

    fn handle_modem_consumer_events(&mut self, events: Vec<ModemConsumerEvent>) {
        for event in events {
            match event {
                ModemConsumerEvent::WriteTerminal(bytes) => self.feed_plain_transport_output(&bytes),
                ModemConsumerEvent::SendServer(bytes) => {
                    let _ = self.write_protocol_bytes(&bytes);
                }
                ModemConsumerEvent::TransferStarted(request) => {
                    if let Some(transfer) = self.modem_consumer.active_transfer().cloned() {
                        self.pending_events
                            .push(TerminalEvent::ModemTransferPrompt { request, transfer });
                    }
                }
                ModemConsumerEvent::TransferDataQueued => {}
                ModemConsumerEvent::TransferCancelRequested => {}
            }
        }
    }

    fn flush_buffered_modem_output(&mut self) -> bool {
        let events = if self.lifecycle.is_running() {
            self.modem_consumer.flush_expired_plain_output()
        } else {
            self.modem_consumer.flush_pending_plain_output()
        };
        let changed = !events.is_empty();
        self.handle_modem_consumer_events(events);
        changed
    }
}

impl TerminalSessionBackend for BleSession {
    fn kind(&self) -> TerminalSessionKind {
        TerminalSessionKind::Ble
    }

    fn title(&self) -> Option<String> {
        Some(self.title.clone().unwrap_or_else(|| self.title_text()))
    }

    fn lifecycle(&self) -> TerminalLifecycle {
        self.lifecycle.clone()
    }

    fn ble_connection_status(&self) -> Option<BleConnectionStatus> {
        // The worker's lifecycle owns link loss: a failed or closed worker
        // releases the subscription and the physical link, so the session can
        // never return to a live state. `connected` only separates the
        // pre-connect phase from a subscribed stream.
        Some(if !self.lifecycle.is_running() {
            BleConnectionStatus::Disconnected
        } else if self.connected {
            BleConnectionStatus::Connected
        } else {
            BleConnectionStatus::Connecting
        })
    }

    fn ble_negotiated_mtu(&self) -> Option<u16> {
        self.negotiated_mtu
    }

    fn process_info(&self) -> TerminalProcessInfo {
        TerminalProcessInfo::default()
    }

    fn refresh_process_info(&mut self) {}

    fn read_pending(&mut self) -> bool {
        self.read_pending_with_budget(TerminalDrainBudget::unlimited())
            .changed
    }

    fn read_pending_with_budget(&mut self, budget: TerminalDrainBudget) -> TerminalDrainReport {
        let started = Instant::now();
        let mut report = self.drain_worker_events_with_budget(budget);
        if self.flush_buffered_modem_output() {
            report.mark_changed();
        }
        if self.flush_modem_server_writes() {
            report.mark_changed();
        }
        while report.events_drained < budget.max_events && !budget.time_exhausted(started) {
            let Ok(event) = self.event_rx.try_recv() else {
                break;
            };
            report.events_drained += 1;
            if self.handle_alacritty_event(event) {
                report.mark_changed();
            }
        }
        if (report.events_drained >= budget.max_events || budget.time_exhausted(started))
            && !self.event_rx.is_empty()
        {
            report.budget_exhausted = true;
        }
        report.drain_duration = started.elapsed();
        report
    }

    fn activity_receiver(&self) -> TerminalActivityReceiver {
        self.event_rx.activity_receiver()
    }

    fn take_events(&mut self) -> Vec<TerminalEvent> {
        std::mem::take(&mut self.pending_events)
    }

    fn write_input(&mut self, bytes: &[u8]) -> Result<()> {
        let encoded = self.encode_user_input(bytes)?;
        self.write_protocol_bytes(&encoded)?;
        if self.runtime_options.local_echo {
            // Local echo is pane feedback, never device output: keep it out of
            // recording and file consumers like the serial backend does.
            self.feed_display_output(&encoded, false);
        }
        Ok(())
    }

    fn write_protocol_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        if self.lifecycle.is_running() && !bytes.is_empty() {
            self.send_command(BleCommand::Data(bytes.to_vec()))?;
        }
        Ok(())
    }

    fn write_text(&mut self, text: &str) -> Result<()> {
        let encoded = self.input_encoder.encode_text(text);
        self.write_input(encoded.as_ref())
    }

    fn paste_text(&mut self, text: &str) -> Result<()> {
        let bytes = self
            .input_encoder
            .encode_paste(text, self.mode().contains(TermMode::BRACKETED_PASTE));
        self.write_input(&bytes)
    }

    fn set_palette(&mut self, palette: TerminalPalette) {
        if self.palette == palette {
            return;
        }
        self.palette = palette;
        // Unchanged rows still hold colors resolved from the old palette.
        self.term.lock().mark_fully_damaged();
        self.term.lock().notify_palette_changed();
    }

    fn set_encoding(&mut self, encoding: TerminalEncoding) {
        if self.encoding == encoding {
            return;
        }
        self.encoding = encoding;
        self.output_decoder.set_encoding(encoding);
        self.output_decoder.reset();
        self.input_encoder.set_encoding(encoding);
        self.encoding_detector.set_encoding(encoding);
    }

    fn set_output_processor(&mut self, processor: Option<TerminalOutputProcessor>) {
        self.output_processor = processor;
        self.output_decoder.reset();
        self.encoding_detector.set_encoding(self.encoding);
    }

    fn set_output_events_enabled(&mut self, enabled: bool) {
        self.output_events_enabled = enabled;
    }

    fn set_trigger_rules(
        &mut self,
        rules: Option<Arc<oxideterm_terminal_triggers::CompiledTriggerSet>>,
    ) {
        self.trigger_stream = rules.map(oxideterm_terminal_triggers::TerminalTriggerStream::new);
    }

    fn serial_runtime_options(&self) -> Option<SerialRuntimeOptions> {
        Some(self.runtime_options)
    }

    fn set_serial_runtime_options(&mut self, options: SerialRuntimeOptions) -> Result<()> {
        if self.runtime_options.display_mode != options.display_mode
            || self.runtime_options.output_line_ending != options.output_line_ending
        {
            // Presentation changes apply only to future bytes, so no partial stream state may leak.
            self.hexdump_offset = 0;
            self.line_ending_ingress.reset();
            self.serial_console_ingress = SerialConsoleIngress::default();
        }
        self.runtime_options = options;
        Ok(())
    }

    fn mode(&self) -> TermMode {
        *self.term.lock().mode()
    }

    fn set_focused(&mut self, focused: bool) -> Result<()> {
        let should_report = {
            let mut term = self.term.lock();
            term.is_focused = focused;
            term.mode().contains(TermMode::FOCUS_IN_OUT)
        };

        if let Some(report) = focus_report_sequence(should_report, focused) {
            self.write_protocol_bytes(report)?;
        }
        Ok(())
    }

    fn resize_with_cell_size(&mut self, resize: TerminalResize) -> Result<()> {
        let grid_changed = self.resize.cols != resize.cols || self.resize.rows != resize.rows;
        self.resize = resize;
        let size = TerminalSize {
            cols: resize.cols,
            rows: resize.rows,
            cell_width: resize.cell_width,
            cell_height: resize.cell_height,
        };
        {
            let mut term = self.term.lock();
            term.resize(size);
            self.shell_integration
                .reflow_command_marks(&term, |event| self.pending_events.push(event));
        }
        if grid_changed {
            if let Some(pending) = &mut self.recording_pending {
                pending.resize(resize.cols as u16, resize.rows as u16);
            }
        }
        Ok(())
    }

    fn scroll_lines(&mut self, delta: i32) {
        if delta != 0 {
            self.term.lock().scroll_display(Scroll::Delta(delta));
        }
    }

    fn scroll_lines_snapshot_incremental(
        &mut self,
        delta: i32,
        previous: &TerminalSnapshot,
    ) -> TerminalSnapshot {
        let mut term = self.term.lock();
        scroll_snapshot_from_term(
            &mut term,
            TerminalSize {
                cols: self.resize.cols,
                rows: self.resize.rows,
                cell_width: self.resize.cell_width,
                cell_height: self.resize.cell_height,
            },
            &self.graphics,
            &self.palette,
            delta,
            previous,
        )
    }

    fn page_up(&mut self) {
        self.term.lock().scroll_display(Scroll::PageUp);
    }

    fn page_down(&mut self) {
        self.term.lock().scroll_display(Scroll::PageDown);
    }

    fn scroll_to_top(&mut self) {
        self.term.lock().scroll_display(Scroll::Top);
    }

    fn scroll_to_bottom(&mut self) {
        self.term.lock().scroll_display(Scroll::Bottom);
    }

    fn scroll_to_display_offset(&mut self, offset: usize) {
        let mut term = self.term.lock();
        let max_offset = term.total_lines().saturating_sub(term.screen_lines());
        let target = offset.min(max_offset);
        let current = term.grid().display_offset();
        let delta = target as i32 - current as i32;
        if delta != 0 {
            term.scroll_display(Scroll::Delta(delta));
        }
    }

    fn search_matches(&self, query: &str) -> Vec<TerminalSearchMatch> {
        let term = self.term.lock();
        search_matches_from_term(&term, self.resize.cols, query)
    }

    fn search_source(&self) -> Option<crate::TerminalSearchSource> {
        Some(crate::TerminalSearchSource::new(
            self.term.clone(),
            self.resize.cols,
        ))
    }

    fn set_selection(&self, selection: Option<crate::TerminalSelectionRange>) {
        crate::selection::set_term_selection(&mut self.term.lock(), selection);
    }

    fn selection(&self) -> Option<crate::TerminalSelectionRange> {
        crate::selection::term_selection(&self.term.lock())
    }

    fn clear_buffer(&mut self) {
        let mut term = self.term.lock();
        clear_terminal_buffer(&mut term);
        self.graphics.clear();
    }

    fn command_output_text(&self, mark: &TerminalCommandMark) -> String {
        let term = self.term.lock();
        command_output_text_from_term(&term, mark)
    }

    fn buffer_text(&self) -> String {
        let term = self.term.lock();
        terminal_buffer_text_from_term(&term, self.resize.cols)
    }

    fn snapshot(&self) -> TerminalSnapshot {
        let term = self.term.lock();
        snapshot_from_term(
            &term,
            TerminalSize {
                cols: self.resize.cols,
                rows: self.resize.rows,
                cell_width: self.resize.cell_width,
                cell_height: self.resize.cell_height,
            },
            &self.graphics,
            &self.palette,
        )
    }

    fn snapshot_incremental(&self, previous: &TerminalSnapshot) -> TerminalSnapshot {
        let mut term = self.term.lock();
        incremental_snapshot_from_term(
            &mut term,
            TerminalSize {
                cols: self.resize.cols,
                rows: self.resize.rows,
                cell_width: self.resize.cell_width,
                cell_height: self.resize.cell_height,
            },
            &self.graphics,
            &self.palette,
            previous,
        )
    }

    fn snapshot_with_display_offset(
        &self,
        display_offset: usize,
        rows: usize,
    ) -> TerminalSnapshot {
        let term = self.term.lock();
        snapshot_from_term_with_display_offset(
            &term,
            TerminalSize {
                cols: self.resize.cols,
                rows: self.resize.rows,
                cell_width: self.resize.cell_width,
                cell_height: self.resize.cell_height,
            },
            &self.graphics,
            &self.palette,
            display_offset,
            rows,
        )
    }

    fn start_modem_transfer(
        &mut self,
        request: TerminalModemTransferRequest,
    ) -> Option<ModemTransfer> {
        self.modem_consumer.start_manual_transfer(request)
    }

    fn interrupt_modem_transfer(&mut self) {
        self.modem_consumer.interrupt_transfer();
    }

    fn finish_modem_transfer(&mut self) {
        let trailing_output = self.modem_consumer.finish_transfer();
        self.feed_plain_transport_output(&trailing_output);
    }

    fn pending_output_flush_delay(&self) -> Option<Duration> {
        self.modem_consumer.pending_plain_output_delay()
    }

    fn terminate_active_task(&mut self) -> Result<()> {
        self.write_protocol_bytes(b"\x03")
    }

    fn kill_active_task(&mut self) -> Result<()> {
        self.write_protocol_bytes(b"\x03")
    }

    fn shutdown(&mut self) {
        if matches!(self.lifecycle, TerminalLifecycle::Closed) {
            return;
        }
        if let Some(sink) = &self.recording_sink {
            sink.interrupt();
        }
        if let Some(operation) = self.connect_audit.take() {
            operation.finish(
                oxideterm_audit::AuditOutcome::Cancelled,
                oxideterm_audit::AuditEvidence::Lifecycle,
                None,
                None,
            );
        }
        if self.connected {
            if let Some(context) = &self.audit {
                context.observe(
                    oxideterm_audit::AuditCategory::Connection,
                    "ble_disconnect",
                    None,
                    oxideterm_audit::AuditOutcome::CancelRequested,
                    oxideterm_audit::AuditEvidence::Dispatch,
                    oxideterm_audit::AuditAuthorization::NotRequired,
                );
            }
            self.connected = false;
        }
        let _ = self.send_command(BleCommand::Close);
        if let Some(runtime) = self.runtime.take() {
            // The worker performs the unsubscribe/disconnect on its private
            // runtime. Dropping the runtime here would cancel that task before
            // it releases the BLE link and notify registration, so give it a
            // bounded window to finish. The wait runs off the UI thread.
            std::thread::spawn(move || {
                runtime.shutdown_timeout(Duration::from_secs(2));
            });
        }
        self.lifecycle = TerminalLifecycle::Closed;
    }
}

/// Selects the notify and write characteristics for the BLE terminal channel.
///
/// Prefers NOTIFY over INDICATE and WRITE_WITHOUT_RESPONSE over WRITE to
/// minimize round-trip latency for interactive byte streams.
fn select_channel_characteristics(
    characteristics: &std::collections::BTreeSet<btleplug::api::Characteristic>,
) -> Option<(
    &btleplug::api::Characteristic,
    &btleplug::api::Characteristic,
    btleplug::api::WriteType,
)> {
    let mut notify_char = None;
    let mut write_char = None;
    let mut write_type = btleplug::api::WriteType::WithoutResponse;
    for characteristic in characteristics {
        let props = characteristic.properties;
        if props.contains(btleplug::api::CharPropFlags::NOTIFY) && notify_char.is_none() {
            notify_char = Some(characteristic);
        }
        if props.contains(btleplug::api::CharPropFlags::WRITE_WITHOUT_RESPONSE) {
            write_char = Some(characteristic);
            write_type = btleplug::api::WriteType::WithoutResponse;
        } else if props.contains(btleplug::api::CharPropFlags::WRITE) && write_char.is_none() {
            write_char = Some(characteristic);
            write_type = btleplug::api::WriteType::WithResponse;
        }
    }
    match (notify_char, write_char) {
        (Some(notify), Some(write)) => Some((notify, write, write_type)),
        _ => None,
    }
}

/// Splits a payload into ATT-sized chunks so a long terminal write does not
/// exceed the negotiated link-layer payload.
async fn write_with_mtu(
    peripheral: &btleplug::platform::Peripheral,
    characteristic: &btleplug::api::Characteristic,
    bytes: &[u8],
    write_type: btleplug::api::WriteType,
    config_mtu: Option<u16>,
) -> Result<(), btleplug::Error> {
    use btleplug::api::Peripheral as _;
    // The OS negotiates the ATT MTU; a configured value only caps it so
    // transfers can be throttled for devices that misbehave at larger sizes.
    let negotiated = peripheral.mtu();
    let mtu = config_mtu.map(|configured| configured.min(negotiated)).unwrap_or(negotiated) as usize;
    let max_chunk = mtu.saturating_sub(BLE_ATT_HEADER_BYTES).max(1);
    for chunk in bytes.chunks(max_chunk) {
        peripheral.write(characteristic, chunk, write_type).await?;
    }
    Ok(())
}

/// Progress milestones reported while resolving a saved BLE profile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BleResolveStage {
    OpeningAdapter,
    Scanning,
    Matching,
    VerifyingService,
    Ready,
}

/// Closes a link opened by this flow for service inspection or a terminal
/// start that failed. A link that predated the call belongs to another
/// consumer and must survive, otherwise a later session would tear it down.
async fn disconnect_temporary_link(
    peripheral: &btleplug::platform::Peripheral,
    was_connected: bool,
) {
    use btleplug::api::Peripheral as _;
    if !was_connected {
        let _ = peripheral.disconnect().await;
    }
}

/// Resolves a persisted BLE profile (adapter preference + device identity +
/// service UUID) into a runtime [`BleSessionConfig`] by scanning the radio and
/// matching. Reuses a pre-existing [`oxideterm_ble::BleAdapterGuard`] when
/// supplied so the scan results it produced stay addressable on all three
/// platforms.
///
/// Matching follows a strict two-tier rule:
///
/// 1. **Primary — MAC + service UUID.** When the profile stores a device MAC
///    (public/static address), scan for a peripheral with that address and
///    verify its GATT service table contains the target UUID. The advertised
///    service list is consulted first as an optimization — if the broadcast
///    already carries the UUID we skip a full `discover_services` round-trip.
///
/// 2. **Fallback — name + service UUID.** When MAC matching is disabled
///    (profile has no MAC, e.g. macOS or devices using resolvable private
///    addresses) or the MAC does not match anything in range, fall back to
///    matching the advertisement name and verify the service UUID the same
///    way (advertisement hint first, full discovery as fallback).
///
/// Returns the runtime config plus the guard so the caller (typically
/// [`run_ble_worker`]) can stop the scan window once the terminal link is up.
pub async fn ble_resolve_profile_to_session(
    guard: Option<oxideterm_ble::BleAdapterGuard>,
    adapter_mac: Option<&str>,
    device_mac: Option<&str>,
    device_name: Option<&str>,
    service_uuid: &str,
    runtime_options: SerialRuntimeOptions,
    mtu: Option<u16>,
    mut on_stage: impl FnMut(BleResolveStage),
) -> Result<(BleSessionConfig, oxideterm_ble::BleAdapterGuard), String> {
    use btleplug::api::Peripheral as _;
    use uuid::Uuid;

    on_stage(BleResolveStage::OpeningAdapter);
    let target_service = Uuid::parse_str(service_uuid)
        .map_err(|_| format!("invalid service UUID: {service_uuid}"))?;

    // Step 0 — adapter selection. Reuse a supplied guard; otherwise pick by
    // stored MAC preference or fall back to the first available adapter.
    let guard_supplied = guard.is_some();
    let (guard, adapter_index) =
        match guard {
            Some(existing) => {
                if let Some(pref_mac) = adapter_mac {
                    // A guard was supplied but its adapter MAC is different
                    // from what the profile wanted — still keep the guard
                    // because the scan is already running on the shared
                    // adapter. Record 0 as a best-effort index; the caller
                    // has the guard so the index is cosmetic.
                    let _ = pref_mac;
                }
                (existing, 0usize)
            }
            None => open_adapter_for_profile(adapter_mac).await?,
        };

    // Step 1 — collect discovered peripherals. A supplied guard already has
    // its discovery window open (the form's scan kept it alive so the selected
    // peripheral stays addressable); re-running `start_scan` there fails on
    // BlueZ with "Operation already in progress", so only a freshly opened
    // adapter performs a real scan window.
    on_stage(BleResolveStage::Scanning);
    let devices = if guard_supplied {
        match guard.discovered_devices().await {
            Ok(devices) => devices,
            Err(error) => {
                // This attempt owns the discovery window; a failed resolve must
                // not leave the radio scanning behind.
                guard.stop_scan().await;
                return Err(format!("BLE device enumeration failed: {error}"));
            }
        }
    } else {
        match guard.scan(BLE_SAVED_PROFILE_SCAN_WINDOW).await {
            Ok(devices) => devices,
            Err(error) => {
                guard.stop_scan().await;
                return Err(format!("BLE scan failed: {error}"));
            }
        }
    };

    // Step 2 — identity match: primary MAC branch first, then name fallback.
    on_stage(BleResolveStage::Matching);
    let matched_device = if let Some(mac) = device_mac {
        devices
            .iter()
            .find(|d| d.address.eq_ignore_ascii_case(mac))
            .or_else(|| {
                device_name.and_then(|name| {
                    devices.iter().find(|d| d.name.as_deref() == Some(name))
                })
            })
    } else {
        device_name.and_then(|name| {
            devices.iter().find(|d| d.name.as_deref() == Some(name))
        })
    };

    let Some(device) = matched_device else {
        let _ = guard.stop_scan().await;
        return Err(format!(
            "no BLE device in range matched the profile ({})",
            device_mac.or(device_name).unwrap_or("no identity clue")
        ));
    };

    // Step 3 — service UUID verification. Fast path uses the advertised
    // service list (no round-trip); slow path connects and discovers the
    // full GATT table when the advertisement hint is missing or incomplete.
    on_stage(BleResolveStage::VerifyingService);
    let uuid_match = if device.advertised_services.contains(&target_service) {
        true
    } else {
        let (peripheral, was_connected) =
            match oxideterm_ble::ble_connect_discovered_device(
                guard.adapter(),
                &device.id,
                BLE_CONNECT_TIMEOUT,
            )
            .await
            {
                Ok(pair) => pair,
                Err(error) => {
                    guard.stop_scan().await;
                    return Err(format!("BLE connect failed: {error}"));
                }
            };
        let has_target = match peripheral.discover_services().await {
            Ok(()) => peripheral
                .services()
                .iter()
                .any(|service| service.uuid == target_service),
            Err(error) => {
                disconnect_temporary_link(&peripheral, was_connected).await;
                guard.stop_scan().await;
                return Err(format!("service discovery failed: {error}"));
            }
        };
        // The verification link is temporary; a link that predated this check
        // belongs to another consumer and must survive, otherwise the worker
        // that follows would have to reconnect a link it does not own.
        disconnect_temporary_link(&peripheral, was_connected).await;
        has_target
    };

    if !uuid_match {
        let _ = guard.stop_scan().await;
        return Err(format!(
            "device matched but does not advertise the target service {service_uuid}"
        ));
    }

    let config = BleSessionConfig {
        adapter_index,
        device_id: device.id.clone(),
        service_uuid: target_service,
        device_label: device
            .name
            .clone()
            .unwrap_or_else(|| device.address.clone()),
        runtime_options,
        mtu,
    };
    on_stage(BleResolveStage::Ready);
    Ok((config, guard))
}

/// Opens a [`oxideterm_ble::BleAdapterGuard`] for the profile, honoring the
/// stored adapter MAC preference. Falls back to the first available adapter
/// when the preference is absent or no adapter matches.
async fn open_adapter_for_profile(
    adapter_mac: Option<&str>,
) -> Result<
    (oxideterm_ble::BleAdapterGuard, usize),
    String,
> {
    let infos = oxideterm_ble::ble_list_adapters()
        .await
        .map_err(|error| format!("BLE adapter enumeration failed: {error}"))?;
    if infos.is_empty() {
        return Err("no Bluetooth adapters available".to_string());
    }

    let index = if let Some(pref) = adapter_mac {
        infos
            .iter()
            .position(|info| {
                info.adapter_mac
                    .as_deref()
                    .map(|mac| mac.eq_ignore_ascii_case(pref))
                    .unwrap_or(false)
            })
            .unwrap_or(0)
    } else {
        0
    };

    let guard = oxideterm_ble::BleAdapterGuard::open(index)
        .await
        .map_err(|error| format!("BLE adapter open failed: {error}"))?;
    Ok((guard, index))
}

async fn run_ble_worker(
    config: BleSessionConfig,
    mut adapter_guard: Option<oxideterm_ble::BleAdapterGuard>,
    mut command_rx: tokio::sync::mpsc::Receiver<BleCommand>,
    worker_tx: crate::backpressure::ByteBoundedSender<BleWorkerEvent>,
) {
    use futures_util::StreamExt;
    use btleplug::api::{Manager as _, Peripheral as _};

    // Reuse the adapter that produced the scan results when the form handed it
    // over; the device object (Linux) or peripheral cache (macOS/Windows) is
    // still valid against it. Fall back to opening a fresh adapter only when
    // the guard was not provided (e.g. tests or future reconnect paths).
    let adapter = if let Some(guard) = adapter_guard.as_ref() {
        guard.adapter().clone()
    } else {
        let manager = match btleplug::platform::Manager::new().await {
            Ok(manager) => manager,
            Err(error) => {
                let _ = worker_tx.send_control(BleWorkerEvent::Failed(error.to_string()));
                return;
            }
        };
        match manager.adapters().await {
            Ok(adapters) => match adapters.into_iter().nth(config.adapter_index) {
                Some(adapter) => adapter,
                None => {
                    let _ = worker_tx.send_control(BleWorkerEvent::Failed(format!(
                        "bluetooth adapter index {} is out of range",
                        config.adapter_index
                    )));
                    return;
                }
            },
            Err(error) => {
                let _ = worker_tx.send_control(BleWorkerEvent::Failed(error.to_string()));
                return;
            }
        }
    };
    // The scan's discovery window stays open through connect: on Linux it keeps
    // the BlueZ object alive, and on macOS/Windows the cache was populated by
    // the scan. Once the link is up the device is no longer temporary, so the
    // guard can be dropped to release the radio.
    let (peripheral, was_connected) = match oxideterm_ble::ble_connect_discovered_device(
        &adapter,
        &config.device_id,
        BLE_CONNECT_TIMEOUT,
    )
    .await
    {
        Ok(pair) => pair,
        Err(error) => {
            // Connect failed before the link was up; release the discovery
            // window so the radio does not stay active for a dead session.
            if let Some(guard) = adapter_guard.take() {
                guard.stop_scan().await;
            }
            let _ = worker_tx.send_control(BleWorkerEvent::Failed(error.to_string()));
            return;
        }
    };
    // The established link makes the device non-temporary, so the discovery
    // window is no longer needed.
    if let Some(guard) = adapter_guard.take() {
        guard.stop_scan().await;
    }
    if let Err(error) = peripheral.discover_services().await {
        // A failed start must not leave the physical link (or the notify
        // registration below) behind: leaked links make every reopened session
        // subscribe to an already-live connection and receive duplicate data.
        disconnect_temporary_link(&peripheral, was_connected).await;
        let _ = worker_tx.send_control(BleWorkerEvent::Failed(error.to_string()));
        return;
    }
    let services = peripheral.services();
    let Some(service) = services
        .iter()
        .find(|service| service.uuid == config.service_uuid)
    else {
        disconnect_temporary_link(&peripheral, was_connected).await;
        let _ = worker_tx.send_control(BleWorkerEvent::Failed(format!(
            "service {} not found",
            config.service_uuid
        )));
        return;
    };
    let Some((notify_char, write_char, write_type)) =
        select_channel_characteristics(&service.characteristics)
    else {
        disconnect_temporary_link(&peripheral, was_connected).await;
        let _ = worker_tx.send_control(BleWorkerEvent::Failed(
            "selected service has no notify+write characteristic pair".to_string(),
        ));
        return;
    };
    let notify_uuid = notify_char.uuid;
    if let Err(error) = peripheral.subscribe(notify_char).await {
        disconnect_temporary_link(&peripheral, was_connected).await;
        let _ = worker_tx.send_control(BleWorkerEvent::Failed(error.to_string()));
        return;
    }
    let _ = worker_tx.send_control(BleWorkerEvent::Connected);

    let mut notifications = match peripheral.notifications().await {
        Ok(stream) => stream,
        Err(error) => {
            let _ = peripheral.unsubscribe(notify_char).await;
            disconnect_temporary_link(&peripheral, was_connected).await;
            let _ = worker_tx.send_control(BleWorkerEvent::Failed(error.to_string()));
            return;
        }
    };
    let mut link_check = tokio::time::interval(BLE_LINK_CHECK_INTERVAL);
    link_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    link_check.tick().await;
    // Emit the negotiated MTU once after subscribe completes; platforms that
    // update the value later will re-emit it on the first link-check tick.
    let mut last_mtu = peripheral.mtu();
    let _ = worker_tx.send_control(BleWorkerEvent::NegotiatedMtu(last_mtu));
    loop {
        tokio::select! {
            _ = link_check.tick() => {
                // `Err` means the platform cannot answer the query; only a
                // definite `false` proves the peer is gone.
                if matches!(peripheral.is_connected().await, Ok(false)) {
                    let _ = worker_tx.send_control(BleWorkerEvent::Closed);
                    break;
                }
                // Some platforms finalise the ATT MTU after subscribe returns;
                // surface late updates so the UI reflects the real value.
                let current_mtu = peripheral.mtu();
                if current_mtu != last_mtu {
                    last_mtu = current_mtu;
                    let _ = worker_tx.send_control(BleWorkerEvent::NegotiatedMtu(current_mtu));
                }
            }
            notification = notifications.next() => {
                match notification {
                    Some(notification) if notification.uuid == notify_uuid => {
                        let data = notification.value;
                        let data_len = data.len();
                        if worker_tx.send_async(BleWorkerEvent::Output(data), data_len).await.is_err() {
                            break;
                        }
                    }
                    Some(_) => {}
                    None => {
                        let _ = worker_tx.send_control(BleWorkerEvent::Closed);
                        break;
                    }
                }
            }
            command = command_rx.recv() => {
                match command {
                    Some(BleCommand::Data(bytes)) => {
                        if let Err(error) =
                            write_with_mtu(&peripheral, write_char, &bytes, write_type, config.mtu).await
                        {
                            let _ = worker_tx
                                .send_control(BleWorkerEvent::Failed(error.to_string()));
                            break;
                        }
                    }
                    Some(BleCommand::Close) | None => {
                        let _ = worker_tx.send_control(BleWorkerEvent::Closed);
                        break;
                    }
                }
            }
        }
    }
    // Every exit path releases the notify registration and the physical link,
    // otherwise the next open reuses a live connection and receives one extra
    // copy of every peer notification per leaked attempt.
    let _ = peripheral.unsubscribe(notify_char).await;
    disconnect_temporary_link(&peripheral, was_connected).await;
}

#[cfg(test)]
mod ble_tests {
    use super::*;

    fn characteristic(uuid: u128, properties: btleplug::api::CharPropFlags) -> btleplug::api::Characteristic {
        btleplug::api::Characteristic {
            uuid: uuid::Uuid::from_u128(uuid),
            service_uuid: uuid::Uuid::from_u128(0x0002),
            properties,
            descriptors: std::collections::BTreeSet::new(),
        }
    }

    #[test]
    fn select_channel_characteristics_prefers_notify_and_write_without_response() {
        let notify = characteristic(0x0001, btleplug::api::CharPropFlags::NOTIFY);
        let indicate = characteristic(0x0003, btleplug::api::CharPropFlags::INDICATE);
        let write = characteristic(0x0004, btleplug::api::CharPropFlags::WRITE);
        let wwr = characteristic(0x0005, btleplug::api::CharPropFlags::WRITE_WITHOUT_RESPONSE);
        let mut characteristics = std::collections::BTreeSet::new();
        characteristics.insert(indicate.clone());
        characteristics.insert(write.clone());
        characteristics.insert(notify.clone());
        characteristics.insert(wwr.clone());
        let (n, w, wt) = select_channel_characteristics(&characteristics).unwrap();
        assert_eq!(n.uuid, notify.uuid);
        assert_eq!(w.uuid, wwr.uuid);
        assert_eq!(wt, btleplug::api::WriteType::WithoutResponse);
    }

    #[test]
    fn select_channel_characteristics_rejects_missing_roles() {
        let read_only = characteristic(0x0001, btleplug::api::CharPropFlags::READ);
        let mut characteristics = std::collections::BTreeSet::new();
        characteristics.insert(read_only);
        assert!(select_channel_characteristics(&characteristics).is_none());
    }
}
