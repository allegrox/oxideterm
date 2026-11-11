use super::*;

/// Workspace-unique overlay anchor claimed by the pane whose toolbar state
/// declares `overlay`. The host owns the anchor, the render gate, the card
/// chrome, and the visibility events; the plugin owns the card content and
/// every semantic attached to the state id.
#[derive(Clone)]
pub(in crate::workspace) struct PluginOverlayAnchor {
    pub plugin_id: String,
    pub action_id: String,
    pub pane_id: PaneId,
    pub window: Option<gpui::AnyWindowHandle>,
    /// Last visibility value delivered to the plugin; the poll only emits
    /// changes so the plugin never receives duplicate notifications.
    pub visible: bool,
    /// Whether a visibility poll loop is serving this anchor's generation.
    /// The single-flight guard in the spawn helper checks it so one anchor
    /// never gets two concurrent loops.
    pub timer_active: bool,
    /// Monotonic counter incremented on each reclaim. The visibility poll loop
    /// captures this at spawn time and exits when it no longer matches, so a
    /// release followed by a reclaim within 1s cannot leave two concurrent
    /// loops polling the same anchor.
    pub generation: u64,
}

/// Generic machinery for plugin-contributed terminal toolbar chips. The
/// manifest (`contributes.terminalToolbar`) is the discovery channel: the host
/// renders one chip per contributed action on every terminal pane, stores the
/// per-pane state id, and reports clicks by `plugin_id:action_id`. State ids
/// are opaque here; a state may declare `overlay`, which turns the chip into
/// the anchor primitive below: the host pins a card to the pane, gates it on
/// focus, and forwards visibility events while the plugin renders the content.
impl WorkspaceApp {
    /// Installed, enabled, and activatable as a process runtime. Mirrors the
    /// connection-plugin predicate; permission review blocks activation.
    pub(in crate::workspace) fn native_plugin_available(&self, plugin_id: &str, cx: &App) -> bool {
        self.plugin_entity
            .read(cx)
            .registry()
            .plugins()
            .iter()
            .any(|plugin| {
                plugin.manifest.id == plugin_id
                    && plugin_host::native_plugin_process_activatable(plugin)
            })
    }

    /// Resolves a contributed toolbar action definition by plugin and action
    /// id; a missing entry means the plugin no longer declares it.
    pub(in crate::workspace) fn plugin_toolbar_definition(
        &self,
        plugin_id: &str,
        action_id: &str,
        cx: &App,
    ) -> Option<plugin_host::NativePluginTerminalToolbarActionDef> {
        self.plugin_entity
            .read(cx)
            .registry()
            .contributions()
            .terminal_toolbar_actions
            .iter()
            .find(|entry| entry.plugin_id == plugin_id && entry.definition.id == action_id)
            .map(|entry| entry.definition.clone())
    }

    /// Current state id one pane reports for an action; `None` means the
    /// action's declared default state.
    pub(in crate::workspace) fn plugin_toolbar_state(
        &self,
        plugin_id: &str,
        action_id: &str,
        pane_id: PaneId,
    ) -> Option<&String> {
        self.plugin_toolbar_states
            .get(&(plugin_id.to_string(), action_id.to_string(), pane_id))
    }

    /// Stores the state one pane reports for a contributed action, runs the
    /// generic overlay lifecycle for `overlay` states, and re-pushes the pane
    /// specs. The state map is the authority; specs are a render mirror.
    pub(in crate::workspace) fn apply_plugin_toolbar_state(
        &mut self,
        plugin_id: &str,
        action_id: &str,
        state_id: &str,
        pane_id: PaneId,
        window_handle: Option<gpui::AnyWindowHandle>,
        cx: &mut Context<Self>,
    ) {
        let Some(definition) = self.plugin_toolbar_definition(plugin_id, action_id, cx) else {
            return;
        };
        self.plugin_toolbar_states.insert(
            (plugin_id.to_string(), action_id.to_string(), pane_id),
            state_id.to_string(),
        );
        let overlay = definition
            .states
            .iter()
            .find(|state| state.id == state_id)
            .is_some_and(|state| state.overlay);
        self.apply_plugin_overlay_lifecycle(
            plugin_id,
            action_id,
            overlay,
            pane_id,
            window_handle,
            cx,
        );
        self.sync_plugin_toolbar_state(cx);
    }

    /// Claims or releases the workspace-unique overlay anchor for the pane's
    /// reported state. A claim preempts a different pane's anchor by demoting
    /// that pane's action to its declared default state; a release happens
    /// when the anchored pane leaves the overlay state. The runtime is
    /// started before `toolbar.overlayOpened` so the plugin process can
    /// receive it, and stopped only after `toolbar.overlayClosed` so a
    /// closing plugin still sees the event.
    fn apply_plugin_overlay_lifecycle(
        &mut self,
        plugin_id: &str,
        action_id: &str,
        overlay: bool,
        pane_id: PaneId,
        window_handle: Option<gpui::AnyWindowHandle>,
        cx: &mut Context<Self>,
    ) {
        let anchored_here = self.plugin_overlay_anchor.as_ref().is_some_and(|anchor| {
            (anchor.plugin_id.as_str(), anchor.action_id.as_str(), anchor.pane_id)
                == (plugin_id, action_id, pane_id)
        });
        if overlay {
            // Any anchor that is not this exact action's pane is displaced by
            // the claim, including a different overlay action on the same pane.
            let preempted = if anchored_here {
                None
            } else {
                self.plugin_overlay_anchor.clone()
            };
            if let Some(previous) = &preempted {
                // The preempted pane falls back to its declared default so the
                // chip mirror and the runtime agree with the lost anchor; a
                // definition removed by a plugin update has nothing to demote.
                if let Some(definition) =
                    self.plugin_toolbar_definition(&previous.plugin_id, &previous.action_id, cx)
                {
                    self.plugin_toolbar_states.insert(
                        (
                            previous.plugin_id.clone(),
                            previous.action_id.clone(),
                            previous.pane_id,
                        ),
                        definition.default_state,
                    );
                }
                self.plugin_overlay_anchor = None;
            }
            let claimed = self.plugin_overlay_anchor.is_none();
            if claimed {
                // A fresh generation retires the previous anchor's poll loop:
                // it observes the mismatch and exits, so a release followed by
                // a quick reclaim cannot accumulate concurrent loops. The new
                // loop is spawned below through the single-flight guard.
                self.plugin_overlay_generation += 1;
                self.plugin_overlay_anchor = Some(PluginOverlayAnchor {
                    plugin_id: plugin_id.to_string(),
                    action_id: action_id.to_string(),
                    pane_id,
                    window: window_handle,
                    visible: false,
                    timer_active: false,
                    generation: self.plugin_overlay_generation,
                });
            }
            self.sync_plugin_toolbar_runtime(plugin_id, cx);
            if let Some(previous) = &preempted {
                // The preempted plugin must learn its overlay closed before
                // its runtime may stop, mirroring the release path ordering;
                // otherwise a runtime still engaged by another pane keeps
                // polling for a pane that no longer owns the anchor.
                self.dispatch_native_plugin_event(
                    previous.plugin_id.clone(),
                    plugin_host::NATIVE_PLUGIN_TOOLBAR_OVERLAY_CLOSED_EVENT,
                    serde_json::json!({
                        "actionId": previous.action_id,
                        "paneId": previous.pane_id.0,
                    }),
                    cx,
                );
                if previous.plugin_id != plugin_id {
                    self.sync_plugin_toolbar_runtime(&previous.plugin_id, cx);
                }
            }
            if claimed {
                self.dispatch_native_plugin_event(
                    plugin_id.to_string(),
                    plugin_host::NATIVE_PLUGIN_TOOLBAR_OVERLAY_OPENED_EVENT,
                    serde_json::json!({ "actionId": action_id, "paneId": pane_id.0 }),
                    cx,
                );
                self.ensure_plugin_overlay_visibility_timer(cx);
            }
        } else if anchored_here {
            if let Some(anchor) = self.plugin_overlay_anchor.take() {
                self.dispatch_native_plugin_event(
                    anchor.plugin_id,
                    plugin_host::NATIVE_PLUGIN_TOOLBAR_OVERLAY_CLOSED_EVENT,
                    serde_json::json!({ "actionId": anchor.action_id, "paneId": pane_id.0 }),
                    cx,
                );
            }
            self.sync_plugin_toolbar_runtime(plugin_id, cx);
        } else {
            self.sync_plugin_toolbar_runtime(plugin_id, cx);
        }
    }

    /// Starts or stops a toolbar plugin's runtime from the authoritative
    /// state map: engaged means at least one pane reports a state other than
    /// the action's declared default. Entries whose definition disappeared
    /// (plugin update) are ignored, matching the chip render fallback.
    fn sync_plugin_toolbar_runtime(&mut self, plugin_id: &str, cx: &mut Context<Self>) {
        let engaged =
            self.plugin_toolbar_states
                .iter()
                .any(|((entry_plugin, action_id, _), state)| {
                    entry_plugin == plugin_id
                        && self
                            .plugin_toolbar_definition(entry_plugin, action_id, cx)
                            .is_some_and(|definition| state != &definition.default_state)
                });
        let resolver = self.native_plugin_host_api_resolver(cx);
        self.plugin_entity.update(cx, |plugins, _cx| {
            if engaged {
                let _ = plugins.start_runtime_plugin_activation(plugin_id, resolver);
            } else {
                plugins.stop_runtime_plugin(plugin_id);
            }
        });
    }

    /// Poll-side mirror of the render gate. The card only paints in the
    /// anchor window, and `anchor.window` is written only at claim time: after
    /// the pane moves to another window the gate fails there while the pane
    /// can still be the active one, so the poll must also require the anchor
    /// to point at the window that renders it. Otherwise the plugin keeps
    /// polling the device for a card the user cannot see.
    fn plugin_overlay_poll_visible(&self, cx: &App) -> bool {
        let Some(anchor) = self.plugin_overlay_anchor.as_ref() else {
            return false;
        };
        if self.active_pane_id(cx) != Some(anchor.pane_id) {
            return false;
        }
        let Some(anchor_window) = anchor.window else {
            return false;
        };
        let Some(main) = self.window_registry.handle_for_role(window_registry::WindowRole::Main)
        else {
            return false;
        };
        anchor_window.window_id() == main.window_id()
    }

    /// Single-flight 1s poll that reports focus changes of the anchored pane
    /// to the plugin as `toolbar.overlayVisibilityChanged`. The loop owns no
    /// business semantics and self-terminates once the anchor is gone or its
    /// generation was superseded by a newer claim, so at most one loop is
    /// live per anchor generation.
    fn ensure_plugin_overlay_visibility_timer(&mut self, cx: &mut Context<Self>) {
        let Some(anchor) = self.plugin_overlay_anchor.as_mut() else {
            return;
        };
        if anchor.timer_active {
            return;
        }
        anchor.timer_active = true;
        let generation = anchor.generation;
        cx.spawn(async move |this, cx| {
            loop {
                gpui::Timer::after(std::time::Duration::from_secs(1)).await;
                let Ok(keep) = this.update(cx, |workspace, cx| {
                    let Some(anchor) = workspace.plugin_overlay_anchor.as_ref() else {
                        return false;
                    };
                    if anchor.generation != generation {
                        // A newer claim owns the anchor now; its own loop
                        // serves it, so retire this one instead of polling the
                        // replacement anchor from two loops.
                        return false;
                    }
                    // Mirror the render gate (window match included) so a pane
                    // whose anchor window moved away reports invisible.
                    let visible = workspace.plugin_overlay_poll_visible(cx);
                    if visible != anchor.visible {
                        let plugin_id = anchor.plugin_id.clone();
                        let payload = serde_json::json!({
                            "actionId": anchor.action_id,
                            "paneId": anchor.pane_id.0,
                            "visible": visible,
                        });
                        if let Some(anchor) = workspace.plugin_overlay_anchor.as_mut() {
                            anchor.visible = visible;
                        }
                        workspace.dispatch_native_plugin_event(
                            plugin_id,
                            plugin_host::NATIVE_PLUGIN_TOOLBAR_OVERLAY_VISIBILITY_CHANGED_EVENT,
                            payload,
                            cx,
                        );
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

    /// Whether the overlay card may render in this window; the call site
    /// additionally requires the window to be focused, so an unfocused
    /// anchor window shows nothing.
    pub(in crate::workspace) fn plugin_overlay_targets_window(
        &self,
        window: gpui::AnyWindowHandle,
    ) -> bool {
        self.plugin_overlay_anchor
            .as_ref()
            .is_some_and(|anchor| anchor.window == Some(window))
    }

    /// The overlay anchor is alive while its pane still exists; a closed pane
    /// releases the anchor, so the card never follows focus onto another pane.
    pub(in crate::workspace) fn plugin_overlay_visible(&self, cx: &App) -> bool {
        self.plugin_overlay_anchor
            .as_ref()
            .is_some_and(|anchor| self.tab_host.read(cx).panes().contains_key(&anchor.pane_id))
    }

    /// Bottom-right overlay card rendering the anchored action's `overlayTab`
    /// runtime view. Renders nothing until the activated plugin registers its
    /// view; content refresh is the plugin's responsibility.
    pub(in crate::workspace) fn render_plugin_overlay_panel(
        &mut self,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.tokens.ui;
        let Some(anchor) = self.plugin_overlay_anchor.clone() else {
            return div().into_any_element();
        };
        let Some(definition) =
            self.plugin_toolbar_definition(&anchor.plugin_id, &anchor.action_id, cx)
        else {
            return div().into_any_element();
        };
        let Some(tab_id) = definition.overlay_tab.as_deref() else {
            return div().into_any_element();
        };
        let Some(mut view) = self
            .plugin_entity
            .read(cx)
            .registry()
            .contributions()
            .runtime_tab_view(&anchor.plugin_id, tab_id)
        else {
            return div().into_any_element();
        };
        super::plugin_ui::localize_native_plugin_schema(&mut view.schema, self.i18n.locale());
        let title = view
            .schema
            .title
            .clone()
            .unwrap_or_else(|| view.title.clone());
        let body = self.render_native_plugin_declarative_schema(
            &anchor.plugin_id,
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
                            .id("plugin-overlay-close")
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
                                    // Closing the card equals the toolbar chip
                                    // cycling to the default state: it releases
                                    // the anchor and re-derives the runtime.
                                    let Some(anchor) = this.plugin_overlay_anchor.clone() else {
                                        return;
                                    };
                                    let Some(definition) = this.plugin_toolbar_definition(
                                        &anchor.plugin_id,
                                        &anchor.action_id,
                                        cx,
                                    ) else {
                                        return;
                                    };
                                    this.apply_plugin_toolbar_state(
                                        &anchor.plugin_id,
                                        &anchor.action_id,
                                        &definition.default_state,
                                        anchor.pane_id,
                                        anchor.window,
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
            .into_any_element()
    }

    /// Removes every toolbar state entry owned by a closed pane, releases the
    /// overlay anchor it held, and re-derives the runtime of every affected
    /// plugin. Reconnecting opens a fresh pane that seeds the default state.
    pub(in crate::workspace) fn plugin_toolbar_pane_closed(
        &mut self,
        pane_id: &PaneId,
        cx: &mut Context<Self>,
    ) {
        let mut affected = Vec::new();
        self.plugin_toolbar_states
            .retain(|(plugin_id, _, entry_pane), _| {
                if entry_pane == pane_id {
                    if !affected.contains(plugin_id) {
                        affected.push(plugin_id.clone());
                    }
                    false
                } else {
                    true
                }
            });
        if self
            .plugin_overlay_anchor
            .as_ref()
            .is_some_and(|anchor| anchor.pane_id == *pane_id)
        {
            if let Some(anchor) = self.plugin_overlay_anchor.take() {
                self.dispatch_native_plugin_event(
                    anchor.plugin_id,
                    plugin_host::NATIVE_PLUGIN_TOOLBAR_OVERLAY_CLOSED_EVENT,
                    serde_json::json!({ "actionId": anchor.action_id, "paneId": pane_id.0 }),
                    cx,
                );
            }
        }
        for plugin_id in affected {
            self.sync_plugin_toolbar_runtime(&plugin_id, cx);
        }
        self.sync_plugin_toolbar_state(cx);
    }

    /// Locale bundles for the plugins contributing toolbar actions, keyed by
    /// plugin id. Rebuilt on every sync so labels follow locale switches and
    /// package updates without a cache to invalidate.
    fn plugin_toolbar_locale_bundles(
        &self,
        contributions: &[plugin_host::NativePluginTerminalToolbarActionContribution],
        cx: &App,
    ) -> HashMap<String, HashMap<String, String>> {
        let plugin_entity = self.plugin_entity.read(cx);
        let tag = plugin_ui::native_plugin_locale_tag(self.i18n.locale());
        let mut bundles = HashMap::new();
        for entry in contributions {
            if bundles.contains_key(&entry.plugin_id) {
                continue;
            }
            let bundle = plugin_entity
                .registry()
                .plugins()
                .iter()
                .find(|plugin| plugin.manifest.id == entry.plugin_id)
                .and_then(|plugin| {
                    let locales_dir = plugin.manifest.locales.as_deref()?;
                    Some(load_plugin_locale_bundle(
                        &plugin.install_dir,
                        locales_dir,
                        tag,
                    ))
                })
                .unwrap_or_default();
            bundles.insert(entry.plugin_id.clone(), bundle);
        }
        bundles
    }

    /// Re-pushes every pane's contributed chip specs (state mirror + plugin
    /// availability). Also re-runs after plugin runtime state changes and
    /// locale switches so chips follow install/disable/activation failures and
    /// label changes without persistence.
    pub(in crate::workspace) fn sync_plugin_toolbar_state(&mut self, cx: &mut Context<Self>) {
        let contributions = self
            .plugin_entity
            .read(cx)
            .registry()
            .contributions()
            .terminal_toolbar_actions
            .clone();
        let bundles = self.plugin_toolbar_locale_bundles(&contributions, cx);
        let panes = self
            .tab_host
            .read(cx)
            .panes()
            .iter()
            .map(|(pane_id, pane)| (*pane_id, pane.clone()))
            .collect::<Vec<_>>();
        for (pane_id, pane) in panes {
            self.push_plugin_toolbar_specs(pane_id, &pane, &contributions, &bundles, cx);
        }
        cx.notify();
    }

    /// Pushes the chip specs for one pane; used when a pane registers so the
    /// new pane renders the contributed default states immediately.
    pub(in crate::workspace) fn push_plugin_toolbar_specs_for_pane(
        &self,
        pane_id: PaneId,
        pane: &gpui::Entity<TerminalPane>,
        cx: &mut Context<Self>,
    ) {
        let contributions = self
            .plugin_entity
            .read(cx)
            .registry()
            .contributions()
            .terminal_toolbar_actions
            .clone();
        let bundles = self.plugin_toolbar_locale_bundles(&contributions, cx);
        self.push_plugin_toolbar_specs(pane_id, pane, &contributions, &bundles, cx);
    }

    fn push_plugin_toolbar_specs(
        &self,
        pane_id: PaneId,
        pane: &gpui::Entity<TerminalPane>,
        contributions: &[plugin_host::NativePluginTerminalToolbarActionContribution],
        bundles: &HashMap<String, HashMap<String, String>>,
        cx: &mut Context<Self>,
    ) {
        let specs = contributions
            .iter()
            .map(|entry| {
                let def = &entry.definition;
                let available = self.native_plugin_available(&entry.plugin_id, cx);
                let bundle = bundles.get(&entry.plugin_id);
                // A missing key shows the raw key, mirroring the host i18n
                // fallback for untranslated labels.
                let label = |key: &str| {
                    bundle
                        .and_then(|bundle| bundle.get(key))
                        .cloned()
                        .unwrap_or_else(|| key.to_string())
                };
                let state_id = self
                    .plugin_toolbar_state(&entry.plugin_id, &def.id, pane_id)
                    .cloned()
                    .unwrap_or_else(|| def.default_state.clone());
                // Validation guarantees `default_state` is declared, so the
                // fallback only covers a stored state a plugin update removed.
                let state_label_key = def
                    .states
                    .iter()
                    .find(|state| state.id == state_id)
                    .or_else(|| {
                        def.states
                            .iter()
                            .find(|state| state.id == def.default_state)
                    })
                    .map(|state| state.label_key.as_str())
                    .unwrap_or(def.label_key.as_str());
                TerminalToolbarActionSpec {
                    id: format!("{}:{}", entry.plugin_id, def.id).into(),
                    label: format!("{} {}", label(&def.label_key), label(state_label_key)),
                    enabled: available,
                    active: state_id != def.default_state,
                    disabled_hint: if available {
                        None
                    } else {
                        def.disabled_hint_key.as_ref().map(|key| label(key))
                    },
                }
            })
            .collect::<Vec<_>>();
        pane.update(cx, |pane, cx| pane.set_toolbar_actions(specs, cx));
    }
}

/// Reads one plugin's locale bundle for the current app locale. The chip must
/// render while the plugin runtime is stopped, so the host reads the package
/// files directly instead of asking the plugin process; a missing tag file
/// falls back to English and an absent bundle leaves raw keys visible.
fn load_plugin_locale_bundle(
    install_dir: &Path,
    locales_dir: &str,
    tag: &str,
) -> HashMap<String, String> {
    let dir = install_dir.join(locales_dir);
    for candidate in [tag, "en"] {
        let Ok(text) = fs::read_to_string(dir.join(format!("{candidate}.json"))) else {
            continue;
        };
        return parse_plugin_locale_bundle(&text);
    }
    HashMap::new()
}

fn parse_plugin_locale_bundle(text: &str) -> HashMap<String, String> {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .map(|object| {
            object
                .into_iter()
                .filter_map(|(key, value)| value.as_str().map(|text| (key, text.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_plugin_locale_bundle_prefers_current_tag_and_falls_back_to_english() {
        let dir = tempfile::tempdir().unwrap();
        let locales = dir.path().join("locales");
        std::fs::create_dir(&locales).unwrap();
        std::fs::write(
            locales.join("zh-CN.json"),
            r#"{"toolbarButton":"MC 助手","toolbarModeOff":"关闭"}"#,
        )
        .unwrap();
        std::fs::write(
            locales.join("en.json"),
            r#"{"toolbarButton":"MC Helper","toolbarModeOff":"Off"}"#,
        )
        .unwrap();

        let zh = load_plugin_locale_bundle(dir.path(), "locales", "zh-CN");
        assert_eq!(zh.get("toolbarButton").map(String::as_str), Some("MC 助手"));
        assert_eq!(zh.get("toolbarModeOff").map(String::as_str), Some("关闭"));

        let missing_tag = load_plugin_locale_bundle(dir.path(), "locales", "fr-FR");
        assert_eq!(
            missing_tag.get("toolbarButton").map(String::as_str),
            Some("MC Helper")
        );

        assert!(load_plugin_locale_bundle(dir.path(), "absent", "zh-CN").is_empty());
    }
}

#[cfg(test)]
mod overlay_lifecycle_tests {
    use super::*;
    use crate::workspace::session_manager::tests::isolated_workspace_fixture;
    use gpui::TestAppContext;

    /// WorkspaceApp owns process-wide storage, so run the overlay state
    /// machine against an isolated portable fixture directory like the other
    /// workspace-level tests.
    fn overlay_test_workspace(cx: &mut TestAppContext) -> Option<Entity<WorkspaceApp>> {
        let _fixture_dir =
            isolated_workspace_fixture(cx, "OXIDETERM_PLUGIN_OVERLAY_TEST_DIR")?;
        let settings_path = default_settings_path();
        let mut settings = SettingsStore::load_from_path(settings_path).unwrap();
        settings.settings_mut().ssh_config.auto_load_hosts = false;
        settings.settings_mut().onboarding_completed = true;
        settings.save().unwrap();
        // The claim path spawns the visibility poll loop on the workspace.
        cx.executor().allow_parking();
        let (shell, _window_cx) = cx.add_window_view(|window, cx| {
            let workspace = cx.new(|cx| WorkspaceApp::new(window, cx, None, None).unwrap());
            WorkspaceWindowShell::new(workspace, window, cx)
        });
        Some(shell.read_with(cx, |shell, _| shell.session_entity()))
    }

    #[gpui::test]
    fn same_pane_second_action_preempts_and_foreign_release_keeps_anchor(
        cx: &mut TestAppContext,
    ) {
        let Some(workspace) = overlay_test_workspace(cx) else {
            return;
        };
        let pane = PaneId(1);
        workspace.update(cx, |ws, cx| {
            ws.apply_plugin_overlay_lifecycle("p", "a1", true, pane, None, cx);
            let (first_action, first_generation) = {
                let anchor = ws.plugin_overlay_anchor.as_ref().expect("first claim anchors");
                (anchor.action_id.clone(), anchor.generation)
            };
            assert_eq!(first_action, "a1");

            // H1: a second overlay action on the same pane must not silently
            // share the anchor; it preempts and claims with a newer generation.
            ws.apply_plugin_overlay_lifecycle("p", "a2", true, pane, None, cx);
            let second_generation = {
                let anchor = ws.plugin_overlay_anchor.as_ref().expect("second claim anchors");
                assert_eq!(anchor.action_id, "a2");
                assert_eq!(anchor.pane_id, pane);
                anchor.generation
            };
            assert!(second_generation > first_generation);

            // Re-applying the anchored action's own state keeps the anchor.
            ws.apply_plugin_overlay_lifecycle("p", "a2", true, pane, None, cx);
            assert_eq!(
                ws.plugin_overlay_anchor.as_ref().unwrap().generation,
                second_generation
            );

            // H1: a non-anchored action leaving overlay must not release it.
            ws.apply_plugin_overlay_lifecycle("p", "a1", false, pane, None, cx);
            assert_eq!(ws.plugin_overlay_anchor.as_ref().unwrap().action_id, "a2");
        });
    }

    #[gpui::test]
    fn release_then_reclaim_starts_a_new_generation(cx: &mut TestAppContext) {
        let Some(workspace) = overlay_test_workspace(cx) else {
            return;
        };
        let pane = PaneId(2);
        workspace.update(cx, |ws, cx| {
            ws.apply_plugin_overlay_lifecycle("p", "a1", true, pane, None, cx);
            let generation = ws.plugin_overlay_anchor.as_ref().unwrap().generation;
            // The claim path spawned exactly one poll loop for this generation.
            assert!(ws.plugin_overlay_anchor.as_ref().unwrap().timer_active);

            // Release: the anchored action itself leaves the overlay state.
            ws.apply_plugin_overlay_lifecycle("p", "a1", false, pane, None, cx);
            assert!(ws.plugin_overlay_anchor.is_none());

            // H2: a reclaim right after release must carry a newer generation
            // so the retired poll loop cannot serve the replacement anchor.
            ws.apply_plugin_overlay_lifecycle("p", "a1", true, pane, None, cx);
            let anchor = ws.plugin_overlay_anchor.as_ref().expect("reclaim anchors");
            assert!(anchor.generation > generation);
            assert!(anchor.timer_active);
        });
    }

    #[gpui::test]
    fn pane_close_releases_anchor_and_state_entries(cx: &mut TestAppContext) {
        let Some(workspace) = overlay_test_workspace(cx) else {
            return;
        };
        let pane = PaneId(3);
        workspace.update(cx, |ws, cx| {
            ws.apply_plugin_overlay_lifecycle("p", "a1", true, pane, None, cx);
            ws.plugin_toolbar_states
                .insert(("p".to_string(), "a1".to_string(), pane), "overlay".to_string());

            ws.plugin_toolbar_pane_closed(&pane, cx);
            assert!(ws.plugin_overlay_anchor.is_none());
            assert!(ws.plugin_toolbar_states.is_empty());
        });
    }
}
