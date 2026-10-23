// Copyright (C) 2026 AnalyseDeCircuit
// SPDX-License-Identifier: GPL-3.0-only

//! BLE discovery UI for the new-connection form: the transport's form branch
//! (adapter picker, scan table, in-place GATT inspection) plus the scan,
//! inspect and selection actions behind it.

use super::*;
use gpui::InteractiveElement;

impl WorkspaceApp {
    pub(super) fn render_ble_form_branch(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.tokens.ui;
        let Some((
            ble_profile_name,
            notes,
            adapters,
            adapters_loading,
            adapter_index,
            devices,
            scanning,
            scan_started,
            selected_device_id,
            inspecting,
            device_details,
            inspect_error,
            selected_service_uuid,
        )) = self.connection_form_state(cx).form.as_ref().map(|form| {
            (
                form.ble_profile_name.clone(),
                form.notes.clone(),
                form.ble_adapters.clone(),
                form.ble_adapters_loading,
                form.ble_selected_adapter_index,
                form.ble_devices.clone(),
                form.ble_scanning,
                form.ble_scan_started,
                form.ble_selected_device_id.clone(),
                form.ble_inspecting,
                form.ble_device_details.clone(),
                form.ble_inspect_error.clone(),
                form.ble_selected_service_uuid,
            )
        })
        else {
            return div().into_any_element();
        };
        let device_list_focus = self.connection_form_state(cx).ble_device_list_focus.clone();

        let adapter_value = adapters
            .get(adapter_index)
            .map(|adapter| adapter.name.clone())
            .unwrap_or_default();
        let adapter_control = if adapters_loading {
            self.render_connection_hint(self.i18n.t("modals.new_connection.ble_loading_adapters"))
                .into_any_element()
        } else if adapters.is_empty() {
            self.render_connection_hint(self.i18n.t("modals.new_connection.ble_no_adapters"))
                .into_any_element()
        } else {
            self.render_new_connection_select_control(
                NewConnectionSelect::BleAdapter,
                adapter_value,
                false,
                false,
                cx,
            )
        };
        // Scanning needs a live adapter; the button only guards against empty
        // hardware. While a scan is running the button stays enabled and acts
        // as a manual stop instead of locking the user out.
        let scan_disabled = adapters_loading || adapters.is_empty();
        let scan_label = if scanning {
            self.i18n.t("modals.new_connection.ble_stop")
        } else {
            self.i18n.t("modals.new_connection.ble_scan")
        };
        let scan_icon = if scanning {
            Self::render_lucide_icon(LucideIcon::Square, 14.0, rgb(self.tokens.ui.text))
        } else {
            Self::render_lucide_icon(LucideIcon::RefreshCw, 14.0, rgb(self.tokens.ui.text))
        };
        let adapter_available = !adapters_loading && !adapters.is_empty();

        // The results frame exists only once a scan has run or produced rows;
        // an always-present empty frame reads as a broken panel. Its contents
        // stay mutually exclusive so one region owns every scan state.
        let device_list: Option<AnyElement> = if adapter_available
            && (scanning || scan_started || !devices.is_empty())
        {
            let content: AnyElement = if scanning {
                self.render_connection_hint(self.i18n.t("modals.new_connection.ble_scanning"))
                    .into_any_element()
            } else if devices.is_empty() {
                self.render_connection_hint(self.i18n.t("modals.new_connection.ble_no_devices"))
                    .into_any_element()
            } else {
                // Column widths are fixed so mono addresses and pills align
                // across rows; only the device name flexes.
                let colors = oxideterm_gpui_ui::TauriTableColors {
                    header_border: rgb(self.tokens.ui.border),
                    header_bg: rgb(self.tokens.ui.bg_sunken),
                    row_border: rgb(self.tokens.ui.border),
                    row_hover_bg: rgb(self.tokens.ui.bg_hover),
                    row_selected_bg: rgb(self.tokens.ui.bg_active),
                };
                let metrics = oxideterm_gpui_ui::TauriTableMetrics::from_tokens(&self.tokens);
                let header_cell = |flexible: bool, width: f32, label: &str| {
                    let cell = if flexible {
                        div().flex_1().min_w(px(100.0))
                    } else {
                        div().w(px(width)).flex_none()
                    };
                    cell.truncate().child(label.to_string()).into_any_element()
                };
                let header = oxideterm_gpui_ui::tauri_table_header(&self.tokens, colors, metrics)
                    .child(header_cell(
                        true,
                        0.0,
                        &self.i18n.t("modals.new_connection.ble_column_name"),
                    ))
                    .child(header_cell(
                        false,
                        122.0,
                        &self.i18n.t("modals.new_connection.ble_column_address"),
                    ))
                    .child(header_cell(false, 62.0, "RSSI"))
                    .child(header_cell(
                        false,
                        88.0,
                        &self.i18n.t("modals.new_connection.ble_column_actions"),
                    ));
                let text_options =
                    |flexible: bool, width: f32| oxideterm_gpui_ui::TauriTableCellOptions {
                        width,
                        min_width: 100.0,
                        flexible,
                        padding_left: 0.0,
                        primary_text_size: self.tokens.metrics.ui_text_sm,
                        meta_text_size: self.tokens.metrics.ui_text_xs,
                        mono_font: Some(gpui::SharedString::from(self.tokens.metrics.font_family)),
                    };
                let mut body = div();
                for device in devices {
                    let selected = selected_device_id.as_ref() == Some(&device.id);
                    let label = device
                        .name
                        .clone()
                        .unwrap_or_else(|| device.address.clone());
                    let mut row = oxideterm_gpui_ui::tauri_table_row(colors, metrics, selected)
                        .child(oxideterm_gpui_ui::tauri_table_cell(
                            &self.tokens,
                            &text_options(true, 0.0),
                            oxideterm_gpui_ui::TauriTableCellStyle::Primary,
                            label,
                        ))
                        .child(oxideterm_gpui_ui::tauri_table_cell(
                            &self.tokens,
                            &text_options(false, 122.0),
                            oxideterm_gpui_ui::TauriTableCellStyle::MetaMono,
                            device.address.clone(),
                        ));
                    row = row.child(oxideterm_gpui_ui::tauri_table_cell(
                        &self.tokens,
                        &text_options(false, 62.0),
                        oxideterm_gpui_ui::TauriTableCellStyle::Meta,
                        device
                            .rssi
                            .map(|rssi| format!("{rssi} dBm"))
                            .unwrap_or_default(),
                    ));
                    let device_id = device.id.clone();
                    let detail_button = self.workspace_toolbar_action_button(
                        self.i18n.t("modals.new_connection.ble_inspect"),
                        None,
                        ToolbarButtonOptions {
                            button: ButtonOptions {
                                variant: ButtonVariant::Outline,
                                size: ButtonSize::Sm,
                                ..ButtonOptions::default()
                            },
                            ..ToolbarButtonOptions::default()
                        },
                        cx.listener(move |this, _event, _window, cx| {
                            // Selecting the row first anchors the expanding
                            // detail table to the clicked row; without it the
                            // table would render under the previously selected
                            // row. Inspect then clears stale details.
                            this.set_new_connection_ble_device(device_id.clone(), cx);
                            this.inspect_new_connection_ble_device(device_id.clone(), cx);
                            cx.stop_propagation();
                        }),
                    );
                    row = row.child(oxideterm_gpui_ui::tauri_table_checkbox_cell(
                        88.0,
                        detail_button,
                    ));
                    let device_id = device.id.clone();
                    row = row.on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                            this.set_new_connection_ble_device(device_id.clone(), cx);
                            // A second click on the same row reads the GATT
                            // layout over a temporary link; single clicks
                            // keep plain selection semantics.
                            if event.click_count >= 2 {
                                this.inspect_new_connection_ble_device(device_id.clone(), cx);
                            }
                            // Clicking a row parks focus on the list so a
                            // following Enter can inspect the selection.
                            let list_focus =
                                this.connection_form_state(cx).ble_device_list_focus.clone();
                            window.focus(&list_focus, cx);
                            cx.stop_propagation();
                        }),
                    );
                    body = body.child(row);
                    // Expanding details in place keeps the inspected device
                    // visually anchored to its row; the table lives only on
                    // the selected row so other rows stay compact.
                    if selected {
                        // The detail wrapper (with padding) is appended only
                        // when there is actual content; otherwise every selected
                        // row would carry blank spacing below it.
                        let wrapper = |detail: AnyElement| {
                            div()
                                .w_full()
                                .pl(px(24.0))
                                .py(px(self.tokens.spacing.two))
                                .child(detail)
                        };
                        if inspecting {
                            body = body.child(wrapper(
                                self.render_connection_hint(
                                    self.i18n.t("modals.new_connection.ble_inspecting"),
                                )
                                .into_any_element(),
                            ));
                        } else if let Some(error) = &inspect_error {
                            body = body.child(wrapper(
                                div()
                                    .text_size(px(self.tokens.metrics.ui_text_xs))
                                    .text_color(rgb(theme.error))
                                    .child(error.clone())
                                    .into_any_element(),
                            ));
                        } else if let Some(details) = &device_details {
                            let detail_colors = oxideterm_gpui_ui::TauriTableColors {
                                header_border: rgb(self.tokens.ui.border),
                                header_bg: rgb(self.tokens.ui.bg_sunken),
                                row_border: rgb(self.tokens.ui.border),
                                row_hover_bg: rgb(self.tokens.ui.bg_hover),
                                row_selected_bg: rgb(self.tokens.ui.bg_active),
                            };
                            let detail_metrics =
                                oxideterm_gpui_ui::TauriTableMetrics::from_tokens(&self.tokens);
                            let detail_name_width = 140.0;
                            let detail_text = |flexible: bool, width: f32| {
                                oxideterm_gpui_ui::TauriTableCellOptions {
                                    width,
                                    min_width: 60.0,
                                    flexible,
                                    padding_left: 0.0,
                                    primary_text_size: self.tokens.metrics.ui_text_xs,
                                    meta_text_size: self.tokens.metrics.ui_text_xs,
                                    mono_font: Some(gpui::SharedString::from(
                                        self.tokens.metrics.font_family,
                                    )),
                                }
                            };
                            fn property_code(name: &str) -> Option<&'static str> {
                                match name {
                                    "read" => Some("R"),
                                    "write-without-response" => Some("WwR"),
                                    "write" => Some("W"),
                                    "notify" => Some("N"),
                                    _ => None,
                                }
                            }
                            fn short_uuid(uuid: &str) -> &str {
                                uuid.split('-').next().unwrap_or(uuid)
                            }
                            let mut detail_table = div().flex().flex_col().w_full();
                            for service in &details.services {
                                let service_selected = selected_service_uuid == Some(service.uuid);
                                let is_nus =
                                    service.uuid == oxideterm_ble::NORDIC_UART_SERVICE_UUID;
                                // NUS is the only service this terminal can
                                // talk to today; show the acronym directly in
                                // place of the UUID so the row is scannable.
                                let mut service_label = if is_nus {
                                    "NUS".to_string()
                                } else {
                                    oxideterm_ble::service_short_name(&service.uuid)
                                };
                                if !service.primary {
                                    service_label.push_str(" · secondary");
                                }
                                let characteristics: String = service
                                    .characteristics
                                    .iter()
                                    .filter_map(|ch| {
                                        let props: Vec<&str> = ch
                                            .properties
                                            .iter()
                                            .filter_map(|p| property_code(p))
                                            .collect();
                                        if props.is_empty() {
                                            None
                                        } else {
                                            Some(format!(
                                                "{} [{}]",
                                                short_uuid(&ch.uuid.to_string()),
                                                props.join(" ")
                                            ))
                                        }
                                    })
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                let service_uuid_full = service.uuid.to_string();
                                let tokens = self.tokens.clone();
                                let name_id = gpui::ElementId::Name(
                                    format!("ble-svc-name-{}", short_uuid(&service_uuid_full))
                                        .into(),
                                );
                                let name_cell = div()
                                    .id(name_id)
                                    .tooltip(move |_window, cx| {
                                        oxideterm_gpui_ui::tooltip::tooltip_view(
                                            tokens.clone(),
                                            service_uuid_full.clone(),
                                            None,
                                            cx,
                                        )
                                    })
                                    .child(oxideterm_gpui_ui::tauri_table_cell(
                                        &self.tokens,
                                        &detail_text(false, detail_name_width),
                                        oxideterm_gpui_ui::TauriTableCellStyle::MetaMono,
                                        service_label,
                                    ));
                                let characteristics_for_tooltip = characteristics.clone();
                                let tokens = self.tokens.clone();
                                let props_id = gpui::ElementId::Name(
                                    format!(
                                        "ble-svc-props-{}",
                                        short_uuid(&service.uuid.to_string())
                                    )
                                    .into(),
                                );
                                let props_cell = div()
                                    .id(props_id)
                                    .tooltip(move |_window, cx| {
                                        oxideterm_gpui_ui::tooltip::tooltip_view(
                                            tokens.clone(),
                                            characteristics_for_tooltip.clone(),
                                            None,
                                            cx,
                                        )
                                    })
                                    .child(oxideterm_gpui_ui::tauri_table_cell(
                                        &self.tokens,
                                        &detail_text(true, 0.0),
                                        oxideterm_gpui_ui::TauriTableCellStyle::Meta,
                                        characteristics,
                                    ));
                                let detail_row = oxideterm_gpui_ui::tauri_table_row(
                                    detail_colors,
                                    detail_metrics,
                                    service_selected,
                                )
                                .child(name_cell)
                                .child(props_cell);
                                let svc_uuid = service.uuid;
                                let detail_row = detail_row.on_mouse_down(
                                    gpui::MouseButton::Left,
                                    cx.listener(move |this, _event, _window, cx| {
                                        this.set_new_connection_ble_service(svc_uuid, cx);
                                        cx.stop_propagation();
                                    }),
                                );
                                detail_table = detail_table.child(detail_row);
                            }
                            body = body.child(wrapper(detail_table.into_any_element()));
                        }
                    }
                }
                div().child(header).child(body).into_any_element()
            };
            Some(
                div()
                    .id("ble-device-list")
                    .track_focus(&device_list_focus)
                    .on_key_down(
                        cx.listener(|this, event: &gpui::KeyDownEvent, _window, cx| {
                            if event.keystroke.key == "enter" {
                                let selected = this
                                    .connection_form_state(cx)
                                    .form
                                    .as_ref()
                                    .and_then(|form| form.ble_selected_device_id.clone());
                                if let Some(device_id) = selected {
                                    this.inspect_new_connection_ble_device(device_id, cx);
                                }
                                cx.stop_propagation();
                            }
                        }),
                    )
                    .max_h(px(300.0))
                    .overflow_y_scroll()
                    .on_scroll_wheel(|_event, _window, cx| cx.stop_propagation())
                    .flex()
                    .flex_col()
                    .rounded(px(self.tokens.radii.sm))
                    .border_1()
                    .border_color(rgb(theme.border))
                    .p(px(self.tokens.spacing.one))
                    .child(content)
                    .into_any_element(),
            )
        } else {
            None
        };

        // The adapter field mirrors the serial port field: a label row that
        // carries the scan action, with the adapter control directly below it.
        let adapter_row = div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(self.tokens.spacing.three))
            .child(
                div()
                    .text_size(px(self.tokens.metrics.ui_text_sm))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(rgb(self.tokens.ui.text))
                    .child(format!(
                        "{}:",
                        self.i18n.t("modals.new_connection.ble_adapter")
                    )),
            )
            .child(self.workspace_toolbar_action_button(
                scan_label,
                Some(scan_icon),
                ToolbarButtonOptions {
                    button: ButtonOptions {
                        variant: ButtonVariant::Outline,
                        size: ButtonSize::Sm,
                        disabled: scan_disabled,
                        ..ButtonOptions::default()
                    },
                    ..ToolbarButtonOptions::default()
                },
                cx.listener(|this, _event, _window, cx| {
                    // The button toggles: an in-flight scan is stopped manually;
                    // otherwise a fresh discovery window is opened.
                    if this
                        .connection_form_state(cx)
                        .form
                        .as_ref()
                        .map(|form| form.ble_scanning)
                        .unwrap_or(false)
                    {
                        this.stop_new_connection_ble_scan(cx);
                    } else {
                        this.start_new_connection_ble_scan(cx);
                    }
                    cx.stop_propagation();
                }),
            ));

        // The adapter field and the results frame are one visual group, so the
        // body keeps the tighter intra-field rhythm the serial port field uses
        // instead of the wider section-level gap.
        let discovery_body = div()
            .flex()
            .flex_col()
            .gap(px(self.tokens.spacing.two))
            .child(adapter_row)
            .child(adapter_control)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(self.tokens.spacing.two))
                    .when_some(device_list, |column, list| column.child(list))
                    .child(self.render_connection_hint(
                        self.i18n.t("modals.new_connection.ble_scan_devices_hint"),
                    )),
            )
            .into_any_element();

        let basic = div()
            .flex()
            .flex_col()
            .gap(px(self.tokens.metrics.modal_section_gap))
            .child(
                self.render_connection_field(
                    self.i18n.t("modals.new_connection.ble_profile_name"),
                    &ble_profile_name,
                    self.i18n
                        .t("modals.new_connection.ble_profile_name_placeholder"),
                    NewConnectionField::BleProfileName,
                    false,
                    cx,
                ),
            )
            .child(self.render_connection_notes_fields(&notes, cx))
            .into_any_element();

        div()
            .flex()
            .flex_col()
            .gap(px(self.tokens.metrics.modal_section_gap))
            .child(self.render_connection_form_section(ConnectionFormSection::Basic, basic, cx))
            .child(self.render_connection_form_section(
                ConnectionFormSection::BleDiscovery,
                discovery_body,
                cx,
            ))
            // Byte-stream transport options mirror the serial control bar so the
            // initial session state matches what the control bar would toggle.
            .child(self.render_connection_form_section(
                ConnectionFormSection::BleTransportOptions,
                self.render_ble_transport_options(cx),
                cx,
            ))
            // Terminal behavior is shared with the telnet form: both call the
            // same renderer so encoding, key sequences, highlighting, and
            // session logging stay in sync instead of diverging.
            .child(self.render_connection_form_section(
                ConnectionFormSection::Terminal,
                self.render_connection_terminal_options(cx),
                cx,
            ))
            // Appearance is rendered now for parity with the shared form; the
            // values stay in form state until BLE connections can be persisted.
            .child(self.render_connection_form_section(
                ConnectionFormSection::Appearance,
                self.render_connection_appearance_options(cx),
                cx,
            ))
            .into_any_element()
    }

    /// Renders the BLE byte-stream transport options: the same five choices the
    /// serial/BLE control bar exposes (send/display line endings, hex send mode,
    /// hex display mode, local echo) so the session starts with them applied.
    fn render_ble_transport_options(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some((input_ending, output_ending, send_mode, display_mode, local_echo, mtu_draft)) =
            self.connection_form_state(cx)
                .form
                .as_ref()
                .map(|form| {
                    (
                        form.ble_input_line_ending,
                        form.ble_output_line_ending,
                        form.ble_send_mode,
                        form.ble_display_mode,
                        form.ble_local_echo,
                        form.ble_mtu.clone(),
                    )
                })
        else {
            return div().into_any_element();
        };
        let mtu_invalid = !mtu_draft.trim().is_empty()
            && mtu_draft
                .trim()
                .parse::<u16>()
                .map(|value| {
                    !(oxideterm_connections::BLE_ATT_MTU_MIN
                        ..=oxideterm_connections::BLE_ATT_MTU_MAX)
                        .contains(&value)
                })
                .unwrap_or(true);
        let mut select_cell = |select: NewConnectionSelect, label_key: &str, value_label: String| {
            div()
                .flex_1()
                .min_w(px(CONNECTION_TERMINAL_CONTROL_MIN_WIDTH))
                .child(form_field(
                    &self.tokens,
                    self.i18n.t(label_key),
                    self.render_new_connection_select_control(
                        select, value_label, false, false, cx,
                    ),
                ))
                .into_any_element()
        };
        div()
            .flex()
            .flex_col()
            .gap(px(self.tokens.spacing.three))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap(px(self.tokens.spacing.three))
                    .child(select_cell(
                        NewConnectionSelect::BleSendMode,
                        "terminal.serial_control.send_mode",
                        self.connection_send_mode_label(send_mode),
                    ))
                    .child(select_cell(
                        NewConnectionSelect::BleDisplayMode,
                        "terminal.serial_control.display_mode",
                        self.connection_display_mode_label(display_mode),
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap(px(self.tokens.spacing.three))
                    .child(select_cell(
                        NewConnectionSelect::BleInputLineEnding,
                        "terminal.serial_control.line_ending",
                        self.connection_line_ending_label(input_ending),
                    ))
                    .child(select_cell(
                        NewConnectionSelect::BleOutputLineEnding,
                        "terminal.serial_control.output_line_ending",
                        self.connection_line_ending_label(output_ending),
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap(px(self.tokens.spacing.three))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(CONNECTION_TERMINAL_CONTROL_MIN_WIDTH))
                            .child(self.render_connection_checkbox(
                                self.i18n.t("terminal.serial_control.local_echo"),
                                local_echo,
                                |form| form.ble_local_echo = !form.ble_local_echo,
                                cx,
                            )),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(CONNECTION_TERMINAL_CONTROL_MIN_WIDTH))
                            .child(self.render_connection_field(
                                self.i18n.t("modals.new_connection.ble_mtu"),
                                &mtu_draft,
                                self.i18n
                                    .t("modals.new_connection.ble_mtu_placeholder"),
                                NewConnectionField::BleMtu,
                                false,
                                cx,
                            )),
                    ),
            )
            .when(mtu_invalid, |section| {
                section.child(self.render_connection_hint_with_color(
                    self.i18n.t("modals.new_connection.ble_mtu_invalid"),
                    self.tokens.ui.error,
                ))
            })
            .into_any_element()
    }

    pub(in crate::workspace) fn refresh_ble_adapters(&mut self, cx: &mut Context<Self>) {
        // Bumping the generation invalidates any in-flight adapter listing or
        // scan so their results cannot reach a form that moved on.
        let generation = self.update_connection_form_state(cx, |state| {
            let generation = state
                .form
                .as_ref()
                .map(|form| form.ble_scan_generation + 1)
                .unwrap_or_default();
            if let Some(form) = state.form.as_mut() {
                form.ble_scan_generation = generation;
                form.ble_adapters_loading = true;
                form.ble_scanning = false;
                form.ble_scan_started = false;
                form.ble_inspecting = false;
                form.ble_devices.clear();
                form.ble_selected_device_id = None;
                form.ble_device_details = None;
                form.error = None;
            }
            generation
        });
        cx.notify();

        let runtime = self.forwarding_runtime.handle().clone();
        cx.spawn(async move |weak, cx| {
            // Adapter enumeration goes through platform Bluetooth stacks and
            // must stay off the GPUI thread.
            let result = runtime
                .spawn(oxideterm_ble::ble_list_adapters())
                .await
                .map_err(|error| error.to_string())
                .and_then(|inner| inner.map_err(|error| error.to_string()));
            let _ = weak.update(cx, |this, cx| {
                this.update_connection_form_state(cx, |state| {
                    let Some(form) = state.form.as_mut() else {
                        return;
                    };
                    if form.ble_scan_generation != generation {
                        return;
                    }
                    form.ble_adapters_loading = false;
                    match result {
                        Ok(adapters) => {
                            form.ble_adapters = adapters;
                            form.ble_selected_adapter_index = 0;
                        }
                        Err(message) => {
                            form.error = Some(format!(
                                "{}: {message}",
                                this.i18n
                                    .t("modals.new_connection.ble_load_adapters_failed")
                            ));
                        }
                    }
                });
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn stop_new_connection_ble_scan(&mut self, cx: &mut Context<Self>) {
        self.update_connection_form_state(cx, |state| {
            if let Some(form) = state.form.as_mut() {
                // Bumping the generation makes the in-flight scan's write-back
                // a no-op; dropping the guard closes the discovery window so
                // the radio is released immediately.
                form.ble_scan_generation += 1;
                form.ble_scanning = false;
                form.ble_scan_started = false;
                form.ble_adapter_guard = None;
                form.ble_selected_device_id = None;
                form.ble_device_details = None;
                form.ble_inspect_error = None;
                form.ble_selected_service_uuid = None;
                form.error = None;
            }
        });
        cx.notify();
    }

    pub(super) fn start_new_connection_ble_scan(&mut self, cx: &mut Context<Self>) {
        let Some((generation, adapter_index)) = self.update_connection_form_state(cx, |state| {
            state.form.as_mut().map(|form| {
                form.ble_scan_generation += 1;
                form.ble_scanning = true;
                form.ble_scan_started = false;
                form.ble_devices.clear();
                form.ble_selected_device_id = None;
                form.ble_device_details = None;
                form.ble_inspect_error = None;
                form.ble_selected_service_uuid = None;
                form.error = None;
                (form.ble_scan_generation, form.ble_selected_adapter_index)
            })
        }) else {
            return;
        };
        cx.notify();

        let runtime = self.forwarding_runtime.handle().clone();
        cx.spawn(async move |weak, cx| {
            // Radio scanning blocks for the whole discovery window and must
            // stay off the GPUI thread. A fresh guard opens the adapter and
            // owns the discovery window; it replaces any previous guard so
            // the old window closes before a new one opens.
            let result = runtime
                .spawn(async move {
                    let guard = oxideterm_ble::BleAdapterGuard::open(adapter_index).await?;
                    let devices = guard.scan(oxideterm_ble::BLE_SCAN_DURATION).await?;
                    Ok::<_, oxideterm_ble::BleError>((devices, guard))
                })
                .await
                .map_err(|error| error.to_string())
                .and_then(|inner| {
                    inner.map_err(|error: oxideterm_ble::BleError| error.to_string())
                });
            let _ = weak.update(cx, |this, cx| {
                this.update_connection_form_state(cx, |state| {
                    let Some(form) = state.form.as_mut() else {
                        return;
                    };
                    if form.ble_scan_generation != generation {
                        // A newer scan superseded this one; drop the guard so
                        // its discovery window closes instead of leaking.
                        if let Ok((_, guard)) = result.as_ref() {
                            // We cannot await here; dropping the guard closes
                            // the underlying adapter session, which releases
                            // the discovery window on the BlueZ side.
                            let _ = guard;
                        }
                        return;
                    }
                    form.ble_scanning = false;
                    form.ble_scan_started = true;
                    match result {
                        Ok((devices, guard)) => {
                            form.ble_adapter_guard = Some(guard);
                            form.ble_devices = devices.clone();
                            form.ble_selected_device_id = devices
                                .iter()
                                .find(|device| device.name.is_some())
                                .map(|device| device.id.clone());
                        }
                        Err(message) => {
                            form.ble_adapter_guard = None;
                            form.error = Some(format!(
                                "{}: {message}",
                                this.i18n.t("modals.new_connection.ble_scan_failed")
                            ));
                        }
                    }
                });
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn inspect_new_connection_ble_device(
        &mut self,
        device_id: oxideterm_ble::BlePeripheralId,
        cx: &mut Context<Self>,
    ) {
        let Some((generation, adapter)) = self.update_connection_form_state(cx, |state| {
            state.form.as_mut().and_then(|form| {
                // Inspection reuses the adapter that produced the scan results
                // so the device object (Linux) or cache entry (macOS/Windows)
                // is still addressable without re-scanning. Resolve the guard
                // before mutating state: a missing guard (scan never run or
                // released by a transport switch) must not leave the
                // inspecting flag stuck at true.
                let adapter = form
                    .ble_adapter_guard
                    .as_ref()
                    .map(|guard| guard.adapter().clone())?;
                // The shared generation token also drops an in-flight
                // inspection whenever a rescan or adapter switch starts.
                form.ble_scan_generation += 1;
                form.ble_inspecting = true;
                form.ble_device_details = None;
                form.ble_inspect_error = None;
                form.ble_selected_service_uuid = None;
                form.error = None;
                Some((form.ble_scan_generation, adapter))
            })
        }) else {
            return;
        };
        cx.notify();

        let runtime = self.forwarding_runtime.handle().clone();
        cx.spawn(async move |weak, cx| {
            // The inspection link connects, reads the GATT layout, and
            // disconnects again; keep it off the GPUI thread.
            let result = runtime
                .spawn(async move { oxideterm_ble::ble_inspect_device(&adapter, device_id).await })
                .await
                .map_err(|error| error.to_string())
                .and_then(|inner| inner.map_err(|error| error.to_string()));
            let _ = weak.update(cx, |this, cx| {
                this.update_connection_form_state(cx, |state| {
                    let Some(form) = state.form.as_mut() else {
                        return;
                    };
                    if form.ble_scan_generation != generation {
                        return;
                    }
                    form.ble_inspecting = false;
                    match result {
                        Ok(details) => form.ble_device_details = Some(details),
                        Err(message) => {
                            form.ble_inspect_error = Some(format!(
                                "{}: {message}",
                                this.i18n.t("modals.new_connection.ble_inspect_failed")
                            ));
                        }
                    }
                });
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn set_new_connection_ble_adapter(
        &mut self,
        adapter_index: usize,
        cx: &mut Context<Self>,
    ) {
        self.update_connection_form_state(cx, |state| {
            if let Some(form) = state.form.as_mut() {
                form.ble_selected_adapter_index = adapter_index;
                // Results from the previous adapter no longer describe the
                // selection; the orphaned scan write-back is dropped by the
                // generation bump, and its discovery window closes with the
                // guard.
                form.ble_scan_generation += 1;
                form.ble_scanning = false;
                form.ble_scan_started = false;
                form.ble_inspecting = false;
                form.ble_devices.clear();
                form.ble_adapter_guard = None;
                form.ble_selected_device_id = None;
                form.ble_device_details = None;
                form.ble_inspect_error = None;
                form.ble_selected_service_uuid = None;
                form.field_focused = false;
                form.selected_field = None;
                form.error = None;
            }
        });
        self.ime_marked_text = None;
        cx.notify();
    }

    pub(super) fn set_new_connection_ble_device(
        &mut self,
        device_id: oxideterm_ble::BlePeripheralId,
        cx: &mut Context<Self>,
    ) {
        self.update_connection_form_state(cx, |state| {
            if let Some(form) = state.form.as_mut() {
                form.ble_selected_device_id = Some(device_id);
                // Details and the selected service belong to the previously
                // inspected device and would mislead once the selection moves.
                form.ble_device_details = None;
                form.ble_inspect_error = None;
                form.ble_selected_service_uuid = None;
                form.field_focused = false;
                form.selected_field = None;
                form.error = None;
            }
        });
        cx.notify();
    }

    /// Selects a service inside the expanded BLE device details table.
    ///
    /// Only services that expose both notify and write capabilities can back a
    /// terminal, but the selection itself is unconditional so the open button
    /// can derive its enabled state from `ble_selected_service_uuid`.
    pub(super) fn set_new_connection_ble_service(
        &mut self,
        service_uuid: uuid::Uuid,
        cx: &mut Context<Self>,
    ) {
        self.update_connection_form_state(cx, |state| {
            if let Some(form) = state.form.as_mut() {
                form.ble_selected_service_uuid = Some(service_uuid);
                form.ble_inspect_error = None;
                form.error = None;
            }
        });
        cx.notify();
    }
}
