use super::*;

/// Shared by the plugin lifecycle owner (`plugin_entity`) and this feature
/// surface so the id never diverges.
pub(in crate::workspace) const MC_HELPER_PLUGIN_ID: &str = "com.oxideterm.microcon-helper";
const MC_HELPER_TAB_ID: &str = "microcon-helper";

/// Toolbar action id the pane's chip reports on click; the generic pane
/// surface knows nothing beyond this opaque string.
pub(in crate::workspace) const MC_HELPER_TOOLBAR_ACTION_ID: &str = "mc-helper-mode";

/// Tri-state chip mode of one pane's helper surface. Host-owned per-pane
/// state (`mc_helper_pane_modes`); the pane's toolbar spec is only a render
/// mirror pushed from here.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(in crate::workspace) enum McHelperDisplayMode {
    #[default]
    Off,
    Inline,
    Overlay,
}

impl McHelperDisplayMode {
    /// Chip cycle order; Overlay's successor is Off, which is also what a
    /// preempted overlay pane falls back to.
    pub(in crate::workspace) fn next(self) -> Self {
        match self {
            Self::Off => Self::Inline,
            Self::Inline => Self::Overlay,
            Self::Overlay => Self::Off,
        }
    }

    fn label_key(self) -> &'static str {
        match self {
            Self::Off => "terminal.mc_helper.mode_off",
            Self::Inline => "terminal.mc_helper.mode_inline",
            Self::Overlay => "terminal.mc_helper.mode_overlay",
        }
    }
}

impl WorkspaceApp {
    /// Installed, enabled, and activatable as a process runtime. Mirrors the
    /// connection-plugin predicate; permission review blocks activation.
    pub(in crate::workspace) fn mc_helper_plugin_available(&self, cx: &App) -> bool {
        self.plugin_entity
            .read(cx)
            .registry()
            .plugins()
            .iter()
            .any(|plugin| {
                plugin.manifest.id == MC_HELPER_PLUGIN_ID
                    && matches!(
                        plugin.state,
                        plugin_host::NativePluginState::ReadyProcess
                            | plugin_host::NativePluginState::Loading
                            | plugin_host::NativePluginState::Active
                    )
                    && matches!(
                        plugin.runtime_plan,
                        plugin_host::NativePluginRuntimePlan::Process { .. }
                    )
                    && !plugin_host::native_plugin_requires_permission_review(
                        &plugin.manifest,
                        &plugin.runtime_plan,
                        &plugin.config,
                    )
            })
    }

    /// Mode switch entry point. Each pane owns its tri-state chip mode in the
    /// host map; this surface tracks the overlay anchor and derives the plugin
    /// lifecycle: the runtime runs while any pane is off Off and stops once
    /// every pane returns to Off. Overlay is workspace unique: the claiming
    /// pane demotes a previous overlay pane to Off (the chip cycle's successor
    /// of Overlay) and takes over the anchor.
    pub(in crate::workspace) fn set_mc_helper_display_mode(
        &mut self,
        mode: McHelperDisplayMode,
        pane_id: PaneId,
        window_handle: Option<gpui::AnyWindowHandle>,
        cx: &mut Context<Self>,
    ) {
        let available = self.mc_helper_plugin_available(cx);
        if mode == McHelperDisplayMode::Overlay {
            if let Some(previous) = self
                .mc_helper_owner_pane_id
                .filter(|owner| *owner != pane_id)
                && let Some(previous_pane) = self.tab_host.read(cx).panes().get(&previous).cloned()
            {
                // The chip cycle ends Overlay -> Off, so a preempted pane
                // falls back to Off, not Inline.
                self.mc_helper_pane_modes
                    .insert(previous, McHelperDisplayMode::Off);
                self.mc_helper_push_pane_spec(previous, &previous_pane, available, cx);
            }
            self.mc_helper_owner_pane_id = Some(pane_id);
            self.mc_helper_owner_window = window_handle;
            // Start a full cycle when the panel (re)opens.
            self.mc_helper_panel_countdown = Self::MC_HELPER_PANEL_REFRESH_SECS as u8;
        } else if self.mc_helper_owner_pane_id == Some(pane_id) {
            // The overlay owner left Overlay: the unique card has no anchor
            // and hides, while other panes keep their own modes.
            self.mc_helper_owner_pane_id = None;
            self.mc_helper_owner_window = None;
        }
        // The host map is the mode authority; the pane spec is a render
        // mirror. Then derive the runtime lifecycle from the per-pane modes.
        self.mc_helper_pane_modes.insert(pane_id, mode);
        if let Some(pane) = self.tab_host.read(cx).panes().get(&pane_id).cloned() {
            self.mc_helper_push_pane_spec(pane_id, &pane, available, cx);
        }
        let engaged = self.mc_helper_any_pane_engaged();
        let resolver = self.native_plugin_host_api_resolver(cx);
        self.plugin_entity.update(cx, |plugins, _cx| {
            if engaged {
                let _ = plugins.start_runtime_plugin_activation(MC_HELPER_PLUGIN_ID, resolver);
            } else {
                plugins.stop_runtime_plugin(MC_HELPER_PLUGIN_ID);
            }
        });
        if mode == McHelperDisplayMode::Overlay {
            // Bind the opening pane's session first (the toolbar chip belongs
            // to that pane, so it holds focus right now), then fetch telemetry
            // immediately. The panel's self-drawn countdown starts from the
            // same moment, so the host never needs per-second re-renders.
            self.dispatch_native_plugin_command(
                MC_HELPER_PLUGIN_ID.to_string(),
                "microconHelper.bindPanel".to_string(),
                cx,
            );
            self.dispatch_native_plugin_command(
                MC_HELPER_PLUGIN_ID.to_string(),
                "microconHelper.refreshPanel".to_string(),
                cx,
            );
            self.ensure_mc_helper_panel_timer(cx);
        }
        self.sync_mc_helper_pane_state(cx);
    }

    /// Whether any open pane still engages the helper (Inline or Overlay);
    /// the host map is the single source of truth for per-pane modes.
    fn mc_helper_any_pane_engaged(&self) -> bool {
        self.mc_helper_pane_modes
            .values()
            .any(|mode| *mode != McHelperDisplayMode::Off)
    }

    /// A terminal pane closed (manual close, reconnect replacing the session,
    /// or the auto-close after the device link dropped). The overlay anchor
    /// and the pane's mode entry die with it; the runtime stops only when no
    /// surviving pane is engaged — the same rule as the panel close button.
    /// Reconnecting opens a fresh pane that seeds Off and re-enables a mode
    /// explicitly.
    pub(in crate::workspace) fn mc_helper_owner_pane_closed(
        &mut self,
        pane_id: &PaneId,
        cx: &mut Context<Self>,
    ) {
        if self.mc_helper_owner_pane_id.as_ref() == Some(pane_id) {
            self.mc_helper_owner_pane_id = None;
            self.mc_helper_owner_window = None;
        }
        self.mc_helper_pane_modes.remove(pane_id);
        if !self.mc_helper_any_pane_engaged() {
            self.plugin_entity.update(cx, |plugins, _cx| {
                plugins.stop_runtime_plugin(MC_HELPER_PLUGIN_ID)
            });
        }
        self.sync_mc_helper_pane_state(cx);
    }

    pub(in crate::workspace) const MC_HELPER_PANEL_REFRESH_SECS: u64 = 10;

    /// 1s cadence: the host owns the countdown and the refresh fire — the
    /// countdown reaching zero triggers the device poll, so async schema
    /// re-renders (status responses, busy-state changes) can never restart
    /// or desync it. The loop self-terminates when the overlay loses its
    /// anchor (owner pane closed or left Overlay); while the owner pane
    /// merely lost focus the countdown and the device poll both freeze and
    /// the panel hides until the owner regains focus.
    fn ensure_mc_helper_panel_timer(&mut self, cx: &mut Context<Self>) {
        if self.mc_helper_panel_timer_active {
            return;
        }
        self.mc_helper_panel_timer_active = true;
        cx.spawn(async move |this, cx| {
            loop {
                gpui::Timer::after(std::time::Duration::from_secs(1)).await;
                let Ok(keep) = this.update(cx, |workspace, cx| {
                    if !workspace.mc_helper_overlay_visible(cx) {
                        workspace.mc_helper_panel_timer_active = false;
                        return false;
                    }
                    if workspace.active_pane_id(cx) == workspace.mc_helper_owner_pane_id
                        // Park the countdown and the device poll while there is
                        // no live telemetry: the first get_status has not
                        // answered yet, or the link dropped (a failed refresh
                        // and a successful restart both clear panel_status).
                        // Reopening the panel issues a fresh get_status.
                        && workspace
                            .mc_helper_panel_status(cx)
                            .is_some_and(|status| status.is_object())
                    {
                        workspace.mc_helper_panel_countdown -= 1;
                        if workspace.mc_helper_panel_countdown == 0 {
                            workspace.mc_helper_panel_countdown =
                                Self::MC_HELPER_PANEL_REFRESH_SECS as u8;
                            workspace.dispatch_native_plugin_command(
                                MC_HELPER_PLUGIN_ID.to_string(),
                                "microconHelper.refreshPanel".to_string(),
                                cx,
                            );
                        }
                        cx.notify();
                    }
                    true
                }) else {
                    break;
                };
                if !keep {
                    break;
                }
            }
        })
        .detach();
    }

    /// Pushes every pane's helper chip spec (mode mirror + plugin
    /// availability) from host state. Also re-runs after plugin runtime state
    /// changes and locale switches so the chip follows
    /// install/disable/activation failures and label changes without
    /// persistence.
    pub(in crate::workspace) fn sync_mc_helper_pane_state(&mut self, cx: &mut Context<Self>) {
        let available = self.mc_helper_plugin_available(cx);
        let panes = self
            .tab_host
            .read(cx)
            .panes()
            .iter()
            .map(|(pane_id, pane)| (*pane_id, pane.clone()))
            .collect::<Vec<_>>();
        for (pane_id, pane) in panes {
            self.mc_helper_push_pane_spec(pane_id, &pane, available, cx);
        }
        cx.notify();
    }

    /// Builds the helper chip spec for one pane from host state and pushes it;
    /// the chip label follows the current locale. The pane's spec equality
    /// guard keeps redundant pushes from triggering re-renders.
    pub(in crate::workspace) fn mc_helper_push_pane_spec(
        &mut self,
        pane_id: PaneId,
        pane: &gpui::Entity<TerminalPane>,
        available: bool,
        cx: &mut Context<Self>,
    ) {
        let mode = self
            .mc_helper_pane_modes
            .get(&pane_id)
            .copied()
            .unwrap_or_default();
        let label = format!(
            "{} {}",
            self.i18n.t("terminal.mc_helper.button"),
            self.i18n.t(mode.label_key())
        );
        let spec = TerminalToolbarActionSpec {
            id: MC_HELPER_TOOLBAR_ACTION_ID.into(),
            label,
            enabled: available,
            active: mode != McHelperDisplayMode::Off,
            disabled_hint: if available {
                None
            } else {
                Some(
                    self.i18n
                        .t("terminal.mc_helper.unavailable_hint")
                        .to_string(),
                )
            },
        };
        pane.update(cx, |pane, cx| pane.set_toolbar_actions(vec![spec], cx));
    }

    /// The pane currently in Overlay mode; the unique card anchors there and
    /// the render gate additionally requires that pane to hold focus, so the
    /// binding never follows focus. Returns None once the owner pane closed
    /// or stopped being a BLE session, which also hides the panel and stops
    /// the telemetry loop.
    fn mc_helper_panel_owner_pane(&self, cx: &App) -> Option<gpui::Entity<TerminalPane>> {
        let pane_id = self.mc_helper_owner_pane_id?;
        let pane = self.tab_host.read(cx).panes().get(&pane_id)?.clone();
        (pane.read(cx).session_kind() == oxideterm_terminal::TerminalSessionKind::Ble)
            .then_some(pane)
    }

    /// Overlay panel is alive while its owning pane exists as a BLE session.
    pub(in crate::workspace) fn mc_helper_overlay_visible(&self, cx: &App) -> bool {
        self.mc_helper_panel_owner_pane(cx).is_some()
    }

    /// Only the window that hosts the owning pane may render the panel
    /// chrome; the call site additionally requires that window to be focused,
    /// so an unfocused owner window shows nothing.
    pub(in crate::workspace) fn mc_helper_panel_targets_window(
        &self,
        window: gpui::AnyWindowHandle,
    ) -> bool {
        self.mc_helper_owner_window == Some(window)
    }

    /// Bottom-right overlay card rendering the helper's declarative schema.
    /// Renders nothing until the activated plugin registers its view.
    pub(in crate::workspace) fn render_mc_helper_overlay_panel(
        &mut self,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.tokens.ui;
        let Some(mut view) = self
            .plugin_entity
            .read(cx)
            .registry()
            .contributions()
            .runtime_tab_view(MC_HELPER_PLUGIN_ID, MC_HELPER_TAB_ID)
        else {
            return div().into_any_element();
        };
        super::plugin_ui::localize_native_plugin_schema(&mut view.schema, self.i18n.locale());
        let title = view
            .schema
            .title
            .clone()
            .unwrap_or_else(|| view.title.clone());
        let panel_status = view
            .schema
            .panel_status
            .clone()
            .unwrap_or(serde_json::Value::Null);
        let body = self.render_native_plugin_declarative_schema(
            MC_HELPER_PLUGIN_ID,
            "tab",
            &view.tab_id,
            &view.schema,
            cx,
        );
        div()
            .absolute()
            .bottom(px(12.0))
            .right(px(12.0))
            .w(px(340.0))
            .max_h(px(420.0))
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded(px(8.0))
            .border_1()
            .border_color(rgb(theme.border))
            .bg(rgb(theme.bg_panel))
            .shadow_lg()
            // Panel clicks must not fall through to the terminal surface and
            // steal focus; the schema body keeps its own blur/select handling.
            .on_mouse_down(MouseButton::Left, |_, _, cx: &mut App| {
                cx.stop_propagation();
            })
            .on_scroll_wheel(cx.listener(|this, _event, _window, cx| {
                this.close_native_plugin_select(cx);
            }))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_between()
                    .px(px(10.0))
                    .py(px(8.0))
                    .child(
                        div()
                            .text_size(px(self.tokens.metrics.ui_text_sm))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(theme.text))
                            .child(title),
                    )
                    .child(
                        div()
                            .id("mc-helper-overlay-close")
                            .cursor_pointer()
                            .px(px(4.0))
                            .text_size(px(self.tokens.metrics.ui_text_sm))
                            .text_color(rgb(theme.text_muted))
                            .child("×")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _event, window, cx| {
                                    window.prevent_default();
                                    cx.stop_propagation();
                                    // Closing the panel equals the toolbar
                                    // chip cycling to Off: it stops the
                                    // plugin and releases subscriptions.
                                    let (pane_id, window_handle) = match (
                                        this.mc_helper_owner_pane_id,
                                        this.mc_helper_owner_window,
                                    ) {
                                        (Some(pane_id), Some(window_handle)) => {
                                            (pane_id, Some(window_handle))
                                        }
                                        _ => return,
                                    };
                                    this.set_mc_helper_display_mode(
                                        McHelperDisplayMode::Off,
                                        pane_id,
                                        window_handle,
                                        cx,
                                    );
                                }),
                            ),
                    ),
            )
            .child(
                div()
                    // The card's height is content-driven (no definite height),
                    // so the body must size to its content; a flex_1 basis of
                    // zero would collapse it to an empty window frame.
                    .max_h(px(370.0))
                    .overflow_y_scrollbar()
                    .px(px(10.0))
                    .pb(px(10.0))
                    .child(body),
            )
            .child(self.render_mc_helper_panel_status_bar(&panel_status))
            .into_any_element()
    }

    /// Fixed bottom bar: icon-led baud/battery/temperature readouts plus the
    /// host-owned refresh countdown. Icons avoid per-locale labels; missing
    /// telemetry renders a dash placeholder. The countdown stays hidden while
    /// there is no live telemetry (first refresh pending, device unreachable,
    /// or link dropped after a restart) — the timer parks the same way.
    fn render_mc_helper_panel_status_bar(&self, status: &serde_json::Value) -> gpui::Div {
        let muted = rgb(self.tokens.ui.text_muted);
        let text = rgb(self.tokens.ui.text);
        let text_size = px(self.tokens.metrics.ui_text_sm);
        let tokens = self.tokens;
        let countdown = self.mc_helper_panel_countdown;
        let hint_baud = self.i18n.t("terminal.mc_helper.hint_baud");
        let hint_battery = self.i18n.t("terminal.mc_helper.hint_battery");
        let hint_temp = self.i18n.t("terminal.mc_helper.hint_temp");
        let item =
            |id: &'static str, icon: crate::assets::LucideIcon, value: String, hint: String| {
                div()
                    .id(id)
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .tooltip(move |_window, cx| {
                        oxideterm_gpui_ui::tooltip::tooltip_view(tokens, hint.clone(), None, cx)
                    })
                    .child(Self::render_lucide_icon(icon, 13.0, muted))
                    .child(div().text_size(text_size).text_color(text).child(value))
            };
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(10.0))
            .border_t_1()
            .border_color(muted)
            .px(px(10.0))
            .py(px(6.0))
            .child(item(
                "mc-helper-hint-baud",
                crate::assets::LucideIcon::SquareActivity,
                mc_helper_panel_value(status, "baud", |baud| format!("{baud}")),
                hint_baud,
            ))
            .child(item(
                "mc-helper-hint-battery",
                crate::assets::LucideIcon::BatteryFull,
                mc_helper_panel_value(status, "batteryPercent", |percent| format!("{percent:.0}%")),
                hint_battery,
            ))
            .child(item(
                "mc-helper-hint-temp",
                crate::assets::LucideIcon::Thermometer,
                mc_helper_panel_value(status, "temperatureC", |celsius| format!("{celsius:.1}°C")),
                hint_temp,
            ))
            .child(div().flex_1())
            // Plain text from the host's countdown cell: element animations
            // restart whenever async schema re-renders rebuild this subtree.
            .when(status.is_object(), |bar| {
                bar.child(
                    div()
                        .text_size(text_size)
                        .text_color(muted)
                        .child(format!("{countdown}s")),
                )
            })
    }

    /// Telemetry payload currently published by the plugin's panel view;
    /// None before the plugin registers its view. `Value::Null` means the
    /// last get_status failed (device unreachable or link dropped).
    fn mc_helper_panel_status(&self, cx: &App) -> Option<serde_json::Value> {
        self.plugin_entity
            .read(cx)
            .registry()
            .contributions()
            .runtime_tab_view(MC_HELPER_PLUGIN_ID, MC_HELPER_TAB_ID)
            .map(|view| {
                view.schema
                    .panel_status
                    .clone()
                    .unwrap_or(serde_json::Value::Null)
            })
    }
}

fn mc_helper_panel_value(
    status: &serde_json::Value,
    key: &str,
    format: fn(f64) -> String,
) -> String {
    status
        .get(key)
        .and_then(serde_json::Value::as_f64)
        .map(format)
        .unwrap_or_else(|| "–".to_string())
}
