use super::*;

/// Generic machinery for plugin-contributed terminal toolbar chips. The
/// manifest (`contributes.terminalToolbar`) is the discovery channel: the host
/// renders one chip per contributed action on every terminal pane, stores the
/// per-pane state id, and reports clicks by `plugin_id:action_id`. State ids
/// are opaque here; any feature lifecycle bound to an action (e.g. the
/// microcon-helper overlay) lives in that feature's own surface.

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
    /// feature lifecycle bound to that action (if any), and re-pushes the pane
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
        if self
            .plugin_toolbar_definition(plugin_id, action_id, cx)
            .is_none()
        {
            return;
        }
        self.plugin_toolbar_states.insert(
            (plugin_id.to_string(), action_id.to_string(), pane_id),
            state_id.to_string(),
        );
        if plugin_id == mc_helper::MC_HELPER_PLUGIN_ID
            && action_id == mc_helper::MC_HELPER_TOOLBAR_ACTION_ID
        {
            self.apply_mc_helper_toolbar_lifecycle(state_id, pane_id, window_handle, cx);
        }
        self.sync_plugin_toolbar_state(cx);
    }

    /// Removes every toolbar state entry owned by a closed pane.
    pub(in crate::workspace) fn plugin_toolbar_pane_closed(&mut self, pane_id: &PaneId) {
        self.plugin_toolbar_states
            .retain(|(_, _, entry_pane), _| entry_pane != pane_id);
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
        let panes = self
            .tab_host
            .read(cx)
            .panes()
            .iter()
            .map(|(pane_id, pane)| (*pane_id, pane.clone()))
            .collect::<Vec<_>>();
        for (pane_id, pane) in panes {
            self.push_plugin_toolbar_specs(pane_id, &pane, &contributions, cx);
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
        self.push_plugin_toolbar_specs(pane_id, pane, &contributions, cx);
    }

    fn push_plugin_toolbar_specs(
        &self,
        pane_id: PaneId,
        pane: &gpui::Entity<TerminalPane>,
        contributions: &[plugin_host::NativePluginTerminalToolbarActionContribution],
        cx: &mut Context<Self>,
    ) {
        let specs = contributions
            .iter()
            .map(|entry| {
                let def = &entry.definition;
                let available = self.native_plugin_available(&entry.plugin_id, cx);
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
                    label: format!(
                        "{} {}",
                        self.i18n.t(&def.label_key),
                        self.i18n.t(state_label_key)
                    ),
                    enabled: available,
                    active: state_id != def.default_state,
                    disabled_hint: if available {
                        None
                    } else {
                        def.disabled_hint_key
                            .as_ref()
                            .map(|key| self.i18n.t(key).to_string())
                    },
                }
            })
            .collect::<Vec<_>>();
        pane.update(cx, |pane, cx| pane.set_toolbar_actions(specs, cx));
    }
}
