//! Tabs: each has its own open note and back/forward history; the tree is shared and follows
//! the active tab, as in the web app.
use crate::app::{App, TabState};
use crate::keymap::Action;

impl App {
    fn stash_active(&mut self) -> TabState {
        TabState { view: self.view.take(), history: std::mem::take(&mut self.history), hist_idx: self.hist_idx }
    }

    fn load_tab_state(&mut self, t: TabState) {
        let (scroll, mode) = t.view.as_ref().map(|v| (v.scroll, v.mode)).unzip();
        self.history = t.history;
        self.hist_idx = t.hist_idx;
        self.view = None;
        let id = t.view.map(|v| v.id).filter(|id| self.model.notes.contains_key(id))
            .or_else(|| self.rows.first().map(|r| r.note_id.clone()));
        if let Some(id) = id {
            self.open_note(&id, false, true);
            if let (Some(v), Some(sc), Some(m)) = (self.view.as_mut(), scroll, mode) {
                if matches!(v.kind, crate::app::Kind::Html) { v.mode = m; }
                v.scroll = sc;
            }
        }
    }

    pub fn switch_tab(&mut self, to: usize) {
        if to == self.active_tab || to >= self.tabs.len() { return; }
        let cur = self.stash_active();
        self.tabs[self.active_tab] = cur;
        let next = std::mem::take(&mut self.tabs[to]);
        self.active_tab = to;
        self.load_tab_state(next);
    }

    pub fn run_tab_action(&mut self, a: Action) {
        let n = self.tabs.len();
        match a {
            Action::NewTab => {
                // the new tab starts on the current note, with the history so far
                let dup = TabState {
                    view: None,
                    history: self.history.clone(),
                    hist_idx: self.hist_idx,
                };
                let cur = self.stash_active();
                self.tabs[self.active_tab] = cur;
                let at = self.active_tab + 1;
                self.tabs.insert(at, TabState::default());
                self.active_tab = at;
                let id = self.tabs[at - 1].view.as_ref().map(|v| v.id.clone());
                self.history = dup.history;
                self.hist_idx = dup.hist_idx;
                if let Some(id) = id { self.open_note(&id, false, false); }
            }
            Action::CloseTab => {
                if n == 1 { self.status = "this is the last tab (q quits)".into(); return; }
                let closing = self.stash_active();
                self.closed_tabs.push(closing);
                self.closed_tabs.truncate(20);
                self.tabs.remove(self.active_tab);
                self.active_tab = self.active_tab.min(self.tabs.len() - 1);
                let next = std::mem::take(&mut self.tabs[self.active_tab]);
                self.load_tab_state(next);
            }
            Action::ReopenTab => {
                let Some(t) = self.closed_tabs.pop() else { self.status = "no closed tab to reopen".into(); return };
                let cur = self.stash_active();
                self.tabs[self.active_tab] = cur;
                self.tabs.insert(self.active_tab + 1, TabState::default());
                self.active_tab += 1;
                self.load_tab_state(t);
            }
            Action::NextTab => self.switch_tab((self.active_tab + 1) % n),
            Action::PrevTab => self.switch_tab((self.active_tab + n - 1) % n),
            Action::GotoTab(i) => {
                let idx = if i == u8::MAX { n - 1 } else { i as usize };
                if idx < n { self.switch_tab(idx); } else { self.status = format!("no tab {}", idx + 1); }
            }
            _ => {}
        }
    }

    /// Titles for the tab bar: (title, is_active).
    pub fn tab_titles(&self) -> Vec<(String, bool)> {
        (0..self.tabs.len()).map(|i| {
            let id = if i == self.active_tab { self.view.as_ref().map(|v| v.id.clone()) } else { self.tabs[i].view.as_ref().map(|v| v.id.clone()) };
            let t = id.map(|id| self.model.title_of(&id).to_string()).unwrap_or_else(|| "new tab".into());
            (t, i == self.active_tab)
        }).collect()
    }
}
