//! Journal (calendar) notes, ported from `date_notes.ts`, plus the creation-time behaviours they
//! depend on: `child:` attribute inheritance, `~template` content copy, and `#sorted` parents.
use crate::{db::Db, write::*};
use anyhow::{Result, bail};
use chrono::{Datelike, Duration, NaiveDate};
use rusqlite::{OptionalExtension, params};
use std::collections::HashSet;

const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
const WEEKDAYS: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];

fn ordinal(n: u32) -> String {
    let suffix = match (n % 100, n % 10) { (11..=13, _) => "th", (_, 1) => "st", (_, 2) => "nd", (_, 3) => "rd", _ => "th" };
    format!("{n}{suffix}")
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
enum Unit { Year, Quarter, Month, Week, Day }

/// `firstDayOfWeek` (1 = Monday … 7 = Sunday), `firstWeekOfYear` (0 = week with Jan 1, 1 = ISO/first Thursday, 2 = minimum days), `minDaysInFirstWeek`.
#[derive(Clone, Copy, Debug)]
pub struct WeekSettings { pub first_day: i64, pub first_week: i64, pub min_days: i64 }

pub const DEFAULT_WEEK_SETTINGS: WeekSettings = WeekSettings { first_day: 1, first_week: 0, min_days: 4 };

fn iso_weekday(d: NaiveDate) -> i64 { d.weekday().number_from_monday() as i64 }

/// Start of the week containing `d`.
pub fn week_start(d: NaiveDate, first_day: i64) -> NaiveDate {
    d - Duration::days((iso_weekday(d) - first_day + 7) % 7)
}

/// `getFirstDayOfWeek1` from Trilium's week_utils.
pub fn first_day_of_week1(year: i32, s: WeekSettings) -> NaiveDate {
    let jan1 = NaiveDate::from_ymd_opt(year, 1, 1).unwrap_or(NaiveDate::MIN);
    let back = (iso_weekday(jan1) - s.first_day + 7) % 7;
    let containing = jan1 - Duration::days(back);
    match s.first_week {
        0 => containing,
        1 => { let jan4 = NaiveDate::from_ymd_opt(year, 1, 4).unwrap_or(jan1); week_start(jan4, s.first_day) }
        _ => if 7 - back >= s.min_days { containing } else { containing + Duration::days(7) },
    }
}

/// `getWeekInfo`: (week year, week number) of the week containing `d`.
pub fn week_info(d: NaiveDate, s: WeekSettings) -> (i32, u32) {
    let start = week_start(d, s.first_day);
    let mut year = d.year();
    let mut first = first_day_of_week1(year, s);
    if start < first { year -= 1; first = first_day_of_week1(year, s); }
    else {
        let next = first_day_of_week1(year + 1, s);
        if start >= next { year += 1; first = next; }
    }
    (year, ((start - first).num_days() / 7 + 1) as u32)
}

fn week_str(wk: (i32, u32)) -> String { format!("{}-W{:02}", wk.0, wk.1) }

/// `getJournalNoteTitle`. A placeholder is only replaced when it belongs to this note's unit or a larger one;
/// `wk` is the week year/number to use for week placeholders.
fn journal_title(pattern: &str, unit: Unit, d: NaiveDate, wk: (i32, u32)) -> String {
    let month = MONTHS[d.month0() as usize];
    let weekday = WEEKDAYS[d.weekday().num_days_from_monday() as usize];
    let mut reps: Vec<(&str, String)> = vec![("year", if unit == Unit::Week { wk.0 } else { d.year() }.to_string())];
    if unit >= Unit::Quarter {
        let q = (d.month0() / 3 + 1).to_string();
        reps.extend([("quarterNumber", q.clone()), ("shortQuarter", format!("Q{q}"))]);
    }
    if unit >= Unit::Month {
        reps.extend([
            ("isoMonth", d.format("%Y-%m").to_string()), ("monthNumber", d.month().to_string()),
            ("monthNumberPadded", format!("{:02}", d.month())), ("month", month.to_string()),
            ("shortMonth3", month.chars().take(3).collect()), ("shortMonth4", month.chars().take(4).collect()),
        ]);
    }
    if unit >= Unit::Week {
        let n = wk.1;
        reps.extend([
            ("weekNumber", n.to_string()), ("weekNumberPadded", format!("{n:02}")),
            ("shortWeek", format!("W{n}")), ("shortWeek3", format!("W{n:02}")),
        ]);
    }
    if unit == Unit::Day {
        reps.extend([
            ("isoDate", d.format("%Y-%m-%d").to_string()), ("dateNumber", d.day().to_string()),
            ("dateNumberPadded", format!("{:02}", d.day())), ("ordinal", ordinal(d.day())),
            ("weekDay", weekday.to_string()), ("weekDay3", weekday.chars().take(3).collect()), ("weekDay2", weekday.chars().take(2).collect()),
        ]);
    }
    let mut out = pattern.to_string();
    for (k, v) in reps { out = out.replace(&format!("{{{k}}}"), &v); }
    out
}

impl Db {
    pub fn label_value(&self, note_id: &str, name: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT value FROM attributes WHERE noteId=? AND type='label' AND name=? AND isDeleted=0 ORDER BY position LIMIT 1",
            params![note_id, name], |r| r.get(0)).optional()?)
    }

    pub fn has_label(&self, note_id: &str, name: &str) -> Result<bool> { Ok(self.label_value(note_id, name)?.is_some()) }

    /// Own attributes plus inheritable ones from ancestors (templates' attributes are not followed).
    pub fn effective_attributes(&self, note_id: &str) -> Result<Vec<(String, String, String, i64, bool)>> {
        let mut out = Vec::new();
        let mut seen_attr = HashSet::new();
        let mut seen_note = HashSet::new();
        let mut queue = vec![(note_id.to_string(), true)];
        while let Some((n, own)) = queue.pop() {
            if !seen_note.insert(n.clone()) { continue; }
            let mut st = self.conn.prepare("SELECT attributeId,type,name,value,position,isInheritable FROM attributes WHERE noteId=? AND isDeleted=0 ORDER BY position")?;
            let rows: Vec<(String, String, String, String, i64, Option<i64>)> =
                st.query_map([&n], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?.collect::<rusqlite::Result<_>>()?;
            for (id, kind, name, value, pos, inh) in rows {
                let inheritable = inh.unwrap_or(0) != 0;
                if (own || inheritable) && seen_attr.insert(id) { out.push((kind, name, value, pos, inheritable)); }
            }
            let mut st = self.conn.prepare("SELECT parentNoteId FROM branches WHERE noteId=? AND +isDeleted=0")?;
            for p in st.query_map([&n], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()? {
                if p != "root" { queue.push((p, false)); }
            }
        }
        Ok(out)
    }

    /// `copyChildAttributes`: `child:x` attributes of the parent become `x` on a new child.
    pub(crate) fn copy_child_attributes_pub(&self, parent: &str, child: &str, child_type: &str) -> Result<()> { self.copy_child_attributes(parent, child, child_type) }

    fn copy_child_attributes(&self, parent: &str, child: &str, child_type: &str) -> Result<()> {
        let has_chosen_template: bool = self.conn.query_row(
            "SELECT 1 FROM attributes WHERE noteId=? AND type='relation' AND name='template' AND isDeleted=0", [child], |r| r.get::<_, i64>(0)).optional()?.is_some();
        for (kind, name, value, _pos, inheritable) in self.effective_attributes(parent)? {
            let Some(stripped) = name.strip_prefix("child:") else { continue };
            if kind == "relation" && stripped == "template" {
                if has_chosen_template { continue; }
                let t: Option<String> = self.conn.query_row("SELECT type FROM notes WHERE noteId=? AND isDeleted=0", [&value], |r| r.get(0)).optional()?;
                if t.as_deref().is_some_and(|t| t != child_type) { continue; }
            }
            self.add_attribute(child, &kind, stripped, &value, inheritable)?;
            if kind == "relation" && stripped == "template" { self.apply_template(child, &value)?; }
        }
        Ok(())
    }

    /// Handler for a freshly created `~template`: an empty text note takes the template's content and type.
    pub fn apply_template(&self, note_id: &str, template_id: &str) -> Result<()> {
        let tpl: Option<(String, String, Option<String>)> = self.conn.query_row(
            "SELECT type,mime,blobId FROM notes WHERE noteId=? AND isDeleted=0", [template_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
        let Some((ttype, tmime, _)) = tpl else { return Ok(()) };
        let own = self.note_content(note_id)?.unwrap_or_default();
        if own.trim().is_empty() && is_string_note(&ttype, &tmime) {
            if let Some(content) = self.note_content(template_id)?.filter(|c| !c.is_empty()) {
                let blob = self.save_blob_for_note(note_id, content.as_bytes(), true)?;
                let old: Option<String> = self.conn.query_row("SELECT blobId FROM notes WHERE noteId=?", [note_id], |r| r.get(0))?;
                self.conn.execute("UPDATE notes SET blobId=?, type=?, mime=?, dateModified=?, utcDateModified=? WHERE noteId=?",
                    params![blob, ttype, tmime, now_local(), now_utc(), note_id])?;
                if let Some(o) = old.filter(|o| *o != blob) { self.drop_blob_if_unused(&o)?; }
                self.record_entity("notes", "noteId", note_id, "utcDateModified")?;
                // text taken from a protected template doesn't become readable in the clear
                if self.note_protected(template_id)? && !self.note_protected(note_id)? { self.protect_note(note_id, true, false)?; }
            }
        }
        let has_children: bool = self.conn.query_row("SELECT 1 FROM branches WHERE parentNoteId=? AND isDeleted=0 LIMIT 1", [note_id], |r| r.get::<_, i64>(0)).optional()?.is_some();
        if !has_children { self.duplicate_children(template_id, note_id, 0)?; }
        Ok(())
    }

    /// `duplicateSubtreeWithoutRoot`: deep-copies the children of `src` under `dst` (no attachments).
    fn duplicate_children(&self, src: &str, dst: &str, depth: usize) -> Result<()> {
        if depth > 32 { bail!("template tree too deep"); }
        let kids: Vec<(String, Option<String>, i64)> = {
            let mut st = self.conn.prepare("SELECT noteId,prefix,notePosition FROM branches WHERE parentNoteId=? AND isDeleted=0 ORDER BY notePosition")?;
            st.query_map([src], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?
        };
        for (kid, prefix, pos) in kids {
            let (title, ntype, mime, protected): (String, String, String, i64) = self.conn.query_row(
                "SELECT title,type,mime,isProtected FROM notes WHERE noteId=? AND isDeleted=0", [&kid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
            // protected children are copied as plaintext and protected again below (the stored title is ciphertext)
            let title = if protected != 0 { self.key()?; self.decrypt_title(&title) } else { title };
            let content = self.note_content(&kid)?.unwrap_or_default();
            let new_id = self.insert_note_raw(dst, &title, &ntype, &mime, &content, prefix.as_deref(), Some(pos))?;
            if protected != 0 && !self.note_protected(&new_id)? { self.protect_note(&new_id, true, false)?; }
            for (kind, name, value, _p, inh) in self.own_attributes(&kid)? {
                if name == "template" && kind == "relation" { continue; }
                self.add_attribute(&new_id, &kind, &name, &value, inh)?;
            }
            self.duplicate_children(&kid, &new_id, depth + 1)?;
        }
        Ok(())
    }

    fn own_attributes(&self, note_id: &str) -> Result<Vec<(String, String, String, i64, bool)>> {
        let mut st = self.conn.prepare("SELECT type,name,value,position,isInheritable FROM attributes WHERE noteId=? AND isDeleted=0 ORDER BY position")?;
        Ok(st.query_map([note_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get::<_, Option<i64>>(4)?.unwrap_or(0) != 0)))?.collect::<rusqlite::Result<_>>()?)
    }

    /// `sortNotesIfNeeded` for the default `#sorted` (by title) with `sortDirection`.
    pub fn sort_children_if_needed(&self, parent: &str) -> Result<()> {
        let Some(sorted) = self.label_value(parent, "sorted")? else { return Ok(()) };
        if sorted == "off" || !(sorted.is_empty() || sorted == "title") { return Ok(()); } // other criteria not ported yet
        let reverse = self.label_value(parent, "sortDirection")?.is_some_and(|v| v.eq_ignore_ascii_case("desc"));
        let mut kids: Vec<(String, String, i64)> = {
            let mut st = self.conn.prepare(
                "SELECT b.branchId, COALESCE(NULLIF(b.prefix,'') || ' - ', '') || n.title, b.notePosition FROM branches b JOIN notes n ON n.noteId=b.noteId
                 WHERE b.parentNoteId=? AND b.isDeleted=0 AND n.isDeleted=0 AND b.noteId != '_hidden'")?;
            st.query_map([parent], |r| Ok((r.get(0)?, r.get::<_, String>(1)?.to_lowercase(), r.get(2)?)))?.collect::<rusqlite::Result<_>>()?
        };
        kids.sort_by(|a, b| if reverse { b.1.cmp(&a.1) } else { a.1.cmp(&b.1) });
        let (mut pos, mut changed) = (10, false);
        for (branch, _t, old) in &kids {
            if *old != pos { self.conn.execute("UPDATE branches SET notePosition=? WHERE branchId=?", params![pos, branch])?; changed = true; }
            pos += 10;
        }
        if changed { self.put_entity_change("note_reordering", parent, "N/A", &now_utc())?; }
        Ok(())
    }

    // ------------------------------------------------------------ journal

    pub fn calendar_root(&self, create: bool) -> Result<Option<String>> {
        let found: Option<String> = self.conn.query_row(
            "SELECT a.noteId FROM attributes a JOIN notes n ON n.noteId=a.noteId WHERE a.type='label' AND a.name='calendarRoot' AND a.isDeleted=0 AND n.isDeleted=0 LIMIT 1",
            [], |r| r.get(0)).optional()?;
        if found.is_some() || !create { return Ok(found); }
        let id = self.create_note("root", "Calendar", "text", "text/html", "")?;
        self.add_attribute(&id, "label", "calendarRoot", "", false)?;
        self.add_attribute(&id, "label", "sorted", "", false)?;
        Ok(Some(id))
    }

    fn find_dated(&self, label: &str, value: &str, root: &str) -> Result<Option<String>> {
        let mut st = self.conn.prepare(
            "SELECT a.noteId FROM attributes a JOIN notes n ON n.noteId=a.noteId WHERE a.type='label' AND a.name=? AND a.value=? AND a.isDeleted=0 AND n.isDeleted=0")?;
        for id in st.query_map(params![label, value], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()? {
            if self.is_descendant_of(&id, root)? { return Ok(Some(id)); }
        }
        Ok(None)
    }

    pub fn is_descendant_of(&self, note: &str, ancestor: &str) -> Result<bool> {
        let (mut stack, mut seen) = (vec![note.to_string()], HashSet::new());
        while let Some(n) = stack.pop() {
            if n == ancestor { return Ok(true); }
            if !seen.insert(n.clone()) { continue; }
            let mut st = self.conn.prepare("SELECT parentNoteId FROM branches WHERE noteId=? AND +isDeleted=0")?;
            stack.extend(st.query_map([&n], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        Ok(false)
    }

    fn journal_child(&self, root: &str, parent: &str, label: &str, value: &str, title: &str, template_rel: &str) -> Result<String> {
        let id = self.create_note(parent, title, "text", "text/html", "")?;
        self.add_attribute(&id, "label", label, value, false)?;
        self.add_attribute(&id, "label", "sorted", "", false)?;
        let tpl: Option<String> = self.conn.query_row(
            "SELECT value FROM attributes WHERE noteId=? AND type='relation' AND name=? AND isDeleted=0 LIMIT 1", params![root, template_rel], |r| r.get(0)).optional()?;
        if let Some(t) = tpl {
            self.add_attribute(&id, "relation", "template", &t, false)?;
            self.apply_template(&id, &t)?;
        }
        Ok(id)
    }

    pub fn week_settings(&self) -> WeekSettings {
        WeekSettings {
            first_day: self.option_i64("firstDayOfWeek", 1).clamp(1, 7),
            first_week: self.option_i64("firstWeekOfYear", 0),
            min_days: self.option_i64("minDaysInFirstWeek", 4),
        }
    }

    fn pattern(&self, root: &str, name: &str, default: &str) -> String {
        self.label_value(root, name).ok().flatten().filter(|v| !v.is_empty()).unwrap_or_else(|| default.to_string())
    }

    fn year_note_in(&self, root: &str, d: NaiveDate) -> Result<String> {
        let year_str = d.year().to_string();
        if let Some(y) = self.find_dated("yearNote", &year_str, root)? { return Ok(y); }
        let t = journal_title(&self.pattern(root, "yearPattern", "{year}"), Unit::Year, d, week_info(d, self.week_settings()));
        self.journal_child(root, root, "yearNote", &year_str, &t, "yearTemplate")
    }

    /// `getQuarterNote` for `YYYY-Qn`.
    fn quarter_note_in(&self, root: &str, quarter: &str) -> Result<String> {
        if let Some(q) = self.find_dated("quarterNote", quarter, root)? { return Ok(q); }
        let bad = || anyhow::anyhow!("bad quarter '{quarter}', want YYYY-Qn");
        let (y, n) = quarter.split_once("-Q").ok_or_else(bad)?;
        let (y, n): (i32, u32) = (y.parse().map_err(|_| bad())?, n.parse().map_err(|_| bad())?);
        if !(1..=4).contains(&n) || !(1..=9999).contains(&y) { return Err(bad()); }
        let quarter = format!("{y}-Q{n}");
        let quarter = quarter.as_str();
        if let Some(q) = self.find_dated("quarterNote", quarter, root)? { return Ok(q); }
        let start = NaiveDate::from_ymd_opt(y, (n - 1) * 3 + 1, 1).ok_or_else(bad)?;
        let year = self.year_note_in(root, start)?;
        let t = journal_title(&self.pattern(root, "quarterPattern", "Quarter {quarterNumber}"), Unit::Quarter, start, week_info(start, self.week_settings()));
        self.journal_child(root, &year, "quarterNote", quarter, &t, "quarterTemplate")
    }

    fn month_note_in(&self, root: &str, d: NaiveDate) -> Result<String> {
        let month_str = d.format("%Y-%m").to_string();
        if let Some(m) = self.find_dated("monthNote", &month_str, root)? { return Ok(m); }
        let parent = if self.has_label(root, "enableQuarterNote")? {
            self.quarter_note_in(root, &format!("{}-Q{}", d.year(), d.month0() / 3 + 1))?
        } else { self.year_note_in(root, d)? };
        let t = journal_title(&self.pattern(root, "monthPattern", "{monthNumberPadded} - {month}"), Unit::Month, d, week_info(d, self.week_settings()));
        self.journal_child(root, &parent, "monthNote", &month_str, &t, "monthTemplate")
    }

    /// `getWeekNote` for `YYYY-Www`; `None` unless the calendar root has `#enableWeekNote`.
    fn week_note_in(&self, root: &str, week: &str) -> Result<Option<String>> {
        if !self.has_label(root, "enableWeekNote")? { return Ok(None); }
        let bad = || anyhow::anyhow!("bad week '{week}', want YYYY-Www");
        let (y, n) = week.split_once("-W").ok_or_else(bad)?;
        let (y, n): (i32, i64) = (y.parse().map_err(|_| bad())?, n.parse().map_err(|_| bad())?);
        if !(1..=53).contains(&n) || !(1..=9999).contains(&y) { return Err(bad()); }
        let week = format!("{y}-W{n:02}");
        let week = week.as_str();
        if let Some(w) = self.find_dated("weekNote", week, root)? { return Ok(Some(w)); }
        let s = self.week_settings();
        let start = first_day_of_week1(y, s) + Duration::days((n - 1) * 7);
        let end = start + Duration::days(6);
        let month = self.month_note_in(root, start)?;
        let t = journal_title(&self.pattern(root, "weekPattern", "Week {weekNumber}"), Unit::Week, start, (y, n as u32));
        let id = self.journal_child(root, &month, "weekNote", week, &t, "weekTemplate")?;
        // a week that straddles two months shows up in both
        if (start.year(), start.month()) != (end.year(), end.month()) {
            let second = self.month_note_in(root, end)?;
            self.clone_note(&id, &second, None)?;
        }
        Ok(Some(id))
    }

    /// `getWeekNote`: the note for `YYYY-Www` (week numbers follow the user's week options), creating its parents.
    /// Fails if the calendar root doesn't have `#enableWeekNote`.
    pub fn week_note(&self, week: &str) -> Result<String> {
        let root = self.calendar_root(true)?.unwrap();
        let tx = self.begin()?;
        let id = self.week_note_in(&root, week.trim())?;
        tx.commit()?;
        id.ok_or_else(|| anyhow::anyhow!("week notes are not enabled (add the label #enableWeekNote to the calendar root)"))
    }

    pub fn quarter_note(&self, quarter: &str) -> Result<String> {
        let root = self.calendar_root(true)?.unwrap();
        let tx = self.begin()?;
        let id = self.quarter_note_in(&root, quarter.trim())?;
        tx.commit()?;
        Ok(id)
    }

    /// The `YYYY-Www` string of the week containing the date, by the user's week options.
    pub fn week_of(&self, date: &str) -> Result<String> {
        let d = NaiveDate::parse_from_str(date.trim().get(..10).unwrap_or(""), "%Y-%m-%d").map_err(|_| anyhow::anyhow!("bad date '{date}', want YYYY-MM-DD"))?;
        Ok(week_str(week_info(d, self.week_settings())))
    }

    /// `getDayNote`: the note for `YYYY-MM-DD`, creating year/(quarter/)month/(week/)day notes as needed.
    pub fn day_note(&self, date: &str) -> Result<String> {
        let d = NaiveDate::parse_from_str(date.trim().get(..10).unwrap_or(""), "%Y-%m-%d").map_err(|_| anyhow::anyhow!("bad date '{date}', want YYYY-MM-DD"))?;
        let root = self.calendar_root(true)?.unwrap();
        let date_str = d.format("%Y-%m-%d").to_string();
        if let Some(id) = self.find_dated("dateNote", &date_str, &root)? { return Ok(id); }
        let tx = self.begin()?;
        let s = self.week_settings();
        let parent = if self.has_label(&root, "enableWeekNote")? {
            self.week_note_in(&root, &week_str(week_info(d, s)))?.expect("enableWeekNote was just checked")
        } else { self.month_note_in(&root, d)? };
        let t = journal_title(&self.pattern(&root, "datePattern", "{dateNumberPadded} - {weekDay}"), Unit::Day, d, week_info(d, s));
        let day = self.journal_child(&root, &parent, "dateNote", &date_str, &t, "dateTemplate")?;
        tx.commit()?;
        Ok(day)
    }

    /// The day note for `YYYY-MM-DD` under the calendar root, if it already exists (never creates anything).
    pub fn existing_day_note(&self, date: &str) -> Result<Option<String>> {
        match self.calendar_root_id()? { Some(root) => self.find_dated("dateNote", date, &root), None => Ok(None) }
    }

    pub fn calendar_root_id(&self) -> Result<Option<String>> {
        Ok(self.conn.query_row(
            "SELECT a.noteId FROM attributes a JOIN notes n ON n.noteId=a.noteId WHERE a.type='label' AND a.name='calendarRoot' AND a.isDeleted=0 AND n.isDeleted=0 LIMIT 1",
            [], |r| r.get(0)).optional()?)
    }

    /// Dates (`YYYY-MM-DD`) in the given month `YYYY-MM` that have a day note, with the note ids.
    pub fn day_notes_in_month(&self, month: &str) -> Result<Vec<(String, String)>> {
        let Some(root) = self.calendar_root_id()? else { return Ok(vec![]) };
        let mut st = self.conn.prepare(
            "SELECT a.value, a.noteId FROM attributes a JOIN notes n ON n.noteId=a.noteId WHERE a.type='label' AND a.name='dateNote' AND a.isDeleted=0 AND n.isDeleted=0 AND a.value LIKE ?")?;
        let rows: Vec<(String, String)> = st.query_map([format!("{month}-%")], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        let mut out = vec![];
        for (d, id) in rows { if self.is_descendant_of(&id, &root)? { out.push((d, id)); } }
        Ok(out)
    }

    pub fn today_note(&self) -> Result<String> {
        self.day_note(&chrono::Local::now().format("%Y-%m-%d").to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Model;

    fn fixture() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("j.db")).unwrap();
        db.set_state("instanceId", "t").unwrap();
        db.conn.execute("INSERT INTO notes(noteId,title,isProtected,type,mime,blobId,isDeleted,dateCreated,dateModified,utcDateCreated,utcDateModified)
            VALUES('root','root',0,'text','text/html',NULL,0,'x','x','2020-01-01 00:00:00.000Z','2020-01-01 00:00:00.000Z')", []).unwrap();
        db.record_entity("notes", "noteId", "root", "utcDateModified").unwrap();
        (dir, db)
    }

    fn title(db: &Db, id: &str) -> String { db.conn.query_row("SELECT title FROM notes WHERE noteId=?", [id], |r| r.get(0)).unwrap() }

    #[test]
    fn titles_follow_trilium_patterns() {
        let d = NaiveDate::from_ymd_opt(2026, 10, 3).unwrap();
        let wk = week_info(d, DEFAULT_WEEK_SETTINGS);
        assert_eq!(journal_title("{dateNumberPadded} - {weekDay}", Unit::Day, d, wk), "03 - Saturday");
        assert_eq!(journal_title("{monthNumberPadded} - {month}", Unit::Month, d, wk), "10 - October");
        assert_eq!(journal_title("{year}", Unit::Year, d, wk), "2026");
        assert_eq!(journal_title("{ordinal} {shortMonth3} {isoDate}", Unit::Day, d, wk), "3rd Oct 2026-10-03");
        assert_eq!(journal_title("{dateNumber} {monthNumber}", Unit::Month, d, wk), "{dateNumber} 10", "unit-restricted placeholders stay literal");
    }

    #[test]
    fn day_note_builds_hierarchy_once_and_orders_by_title() {
        let (_d, db) = fixture();
        let a = db.day_note("2026-10-12").unwrap();
        let b = db.day_note("2026-10-03").unwrap();
        assert_eq!(db.day_note("2026-10-12").unwrap(), a, "idempotent");
        assert_eq!(title(&db, &a), "12 - Monday");
        let m = Model::load(&db).unwrap();
        let month = &m.parents[&a][0];
        assert_eq!(m.title_of(month), "10 - October");
        let order: Vec<_> = m.children[month].iter().map(|c| m.title_of(&c.note_id).to_string()).collect();
        assert_eq!(order, ["03 - Saturday", "12 - Monday"], "#sorted keeps days in date order");
        assert_eq!(m.path_to(&b).unwrap().iter().map(|i| m.title_of(i)).collect::<Vec<_>>(), ["Calendar", "2026", "10 - October", "03 - Saturday"]);
        for (n, (_ok, bad)) in db.recompute_hashes().unwrap() { assert_eq!(bad, 0, "{n}"); }
    }

    #[test]
    fn child_template_chain_fills_day_notes_like_the_users_setup() {
        let (_d, db) = fixture();
        let tpl = db.create_note("root", "Day Template", "text", "text/html", "<h2>Mood</h2><section class=\"include-note\" data-note-id=\"abc\"></section>").unwrap();
        let root = db.calendar_root(true).unwrap().unwrap();
        // ~child:child:child:template on the calendar root, as in the real dashboard
        db.add_attribute(&root, "relation", "child:child:child:template", &tpl, false).unwrap();
        let day = db.day_note("2026-10-12").unwrap();
        assert_eq!(db.note_content(&day).unwrap().unwrap(), "<h2>Mood</h2><section class=\"include-note\" data-note-id=\"abc\"></section>");
        let rel: Option<String> = db.conn.query_row("SELECT value FROM attributes WHERE noteId=? AND name='template' AND isDeleted=0", [&day], |r| r.get(0)).optional().unwrap();
        assert_eq!(rel.as_deref(), Some(tpl.as_str()));
        // year and month notes got the shorter chains, not the template itself
        let m = Model::load(&db).unwrap();
        let month = &m.parents[&day][0];
        assert!(db.label_value(month, "x").unwrap().is_none());
        let month_chain: Option<String> = db.conn.query_row("SELECT name FROM attributes WHERE noteId=? AND type='relation' AND isDeleted=0", [month], |r| r.get(0)).optional().unwrap();
        assert_eq!(month_chain.as_deref(), Some("child:template"));
        for (n, (_ok, bad)) in db.recompute_hashes().unwrap() { assert_eq!(bad, 0, "{n}"); }
    }

    #[test]
    fn week_numbers_follow_the_week_options() {
        let d = |y, m, dd| NaiveDate::from_ymd_opt(y, m, dd).unwrap();
        // default: week 1 contains Jan 1, weeks start on Monday
        assert_eq!(week_info(d(2025, 12, 30), DEFAULT_WEEK_SETTINGS), (2026, 1));
        assert_eq!(week_info(d(2026, 1, 1), DEFAULT_WEEK_SETTINGS), (2026, 1));
        assert_eq!(week_info(d(2026, 10, 12), DEFAULT_WEEK_SETTINGS), (2026, 42));
        // ISO 8601: week 1 contains the first Thursday
        let iso = WeekSettings { first_day: 1, first_week: 1, min_days: 4 };
        assert_eq!(week_info(d(2021, 1, 3), iso), (2020, 53));
        assert_eq!(week_info(d(2021, 1, 4), iso), (2021, 1));
        for day in 1..=366u32 {
            let date = d(2024, 1, 1) + Duration::days(day as i64 - 1);
            let chrono_iso = date.iso_week();
            assert_eq!(week_info(date, iso), (chrono_iso.year(), chrono_iso.week()), "{date}");
        }
        // Sunday-first with a minimum of 4 days in week 1
        let sun = WeekSettings { first_day: 7, first_week: 2, min_days: 4 };
        assert_eq!(first_day_of_week1(2026, sun), d(2026, 1, 4));
        assert_eq!(week_start(d(2026, 10, 14), 7), d(2026, 10, 11));
    }

    #[test]
    fn week_and_quarter_notes_build_the_extended_hierarchy() {
        let (_d, db) = fixture();
        let root = db.calendar_root(true).unwrap().unwrap();
        assert!(db.week_note("2026-W42").unwrap_err().to_string().contains("enableWeekNote"));
        db.add_attribute(&root, "label", "enableWeekNote", "", false).unwrap();
        db.add_attribute(&root, "label", "enableQuarterNote", "", false).unwrap();
        let day = db.day_note("2026-10-12").unwrap();
        let m = Model::load(&db).unwrap();
        assert_eq!(m.path_to(&day).unwrap().iter().map(|i| m.title_of(i)).collect::<Vec<_>>(),
            ["Calendar", "2026", "Quarter 4", "10 - October", "Week 42", "12 - Monday"]);
        assert_eq!(db.week_note("2026-W42").unwrap(), m.parents[&day][0]);
        assert_eq!(db.week_of("2026-10-12").unwrap(), "2026-W42");
        let month = &m.parents[&m.parents[&day][0]][0];
        assert_eq!(db.quarter_note("2026-Q4").unwrap(), m.parents[month][0]);
        // 2026-W40 runs Sep 28 – Oct 4: one note, placed under both months
        let w40 = db.week_note("2026-W40").unwrap();
        let m = Model::load(&db).unwrap();
        assert_eq!(m.parents[&w40].len(), 2, "straddling week is cloned into the second month");
        let months: Vec<_> = m.parents[&w40].iter().map(|p| m.title_of(p).to_string()).collect();
        assert!(months.contains(&"09 - September".to_string()) && months.contains(&"10 - October".to_string()), "{months:?}");
        // the Sep 30 day lives in W40, and its month is September's quarter (Q3)
        let sep30 = db.day_note("2026-09-30").unwrap();
        assert_eq!(db.week_of("2026-09-30").unwrap(), "2026-W40");
        let m = Model::load(&db).unwrap();
        assert_eq!(m.parents[&sep30][0], w40);
        assert!(db.week_note("2026-W99").is_err() && db.quarter_note("2026-Q5").is_err() && db.quarter_note("x").is_err());
        assert_eq!(db.day_note("2026-10-12").unwrap(), day, "idempotent");
        db.set_option("firstWeekOfYear", "1", true).unwrap();
        assert_eq!(db.week_of("2021-01-03").unwrap(), "2020-W53");
        for (n, (_ok, bad)) in db.recompute_hashes().unwrap() { assert_eq!(bad, 0, "{n}"); }
    }

    #[test]
    fn week_and_quarter_labels_are_canonical_and_years_bounded() {
        let (_d, db) = fixture();
        let root = db.calendar_root(true).unwrap().unwrap();
        db.add_attribute(&root, "label", "enableWeekNote", "", false).unwrap();
        let a = db.week_note("2026-W5").unwrap();
        assert_eq!(db.week_note("2026-W05").unwrap(), a);
        assert_eq!(db.week_note("+2026-W05").unwrap(), a);
        let q = db.quarter_note("2026-Q3").unwrap();
        assert_eq!(db.quarter_note("+2026-Q3").unwrap(), q);
        for bad in ["300000-W01", "262142-W53", "0-W01", "-5-W01", "2026-W00", "2026-W54", "x", "２０２６-W01", ""] { assert!(db.week_note(bad).is_err(), "{bad}"); }
        for bad in ["300000-Q1", "0-Q1", "2026-Q0", "2026-Q5"] { assert!(db.quarter_note(bad).is_err(), "{bad}"); }
    }
}
