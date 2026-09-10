//! What the sidebar shows, worked out before anything is drawn.
//!
//! Three levels — machine, working directory, agent — but only two of them
//! are addressable: folders are numbered straight through the whole sidebar,
//! so `^Space 3 1` means folder three whichever machine it turns out to be
//! on, and the host row stays a heading rather than another digit to type.
//!
//! A machine that stops answering keeps its rows, drawn from the last thing
//! warren saw there. Those rows are ghosts: visible, dimmed, not focusable,
//! and never mistaken for something you could type into.

use std::collections::HashSet;

/// One live agent, as the tree needs it.
pub struct Entry<'a> {
    /// Index into the dashboard's agents.
    pub index: usize,
    /// ssh destination, or None for this machine.
    pub host: Option<&'a str>,
    pub cwd: &'a str,
}

/// One machine, as the tree needs it.
pub struct HostView<'a> {
    pub dest: &'a str,
    pub label: &'a str,
    /// None while it is answering; otherwise what to say on its row.
    pub status: Option<String>,
    /// Rows kept from when it last answered, used only while it is not.
    pub ghosts: &'a [crate::remote::Ghost],
}

pub struct Section {
    /// Index into the host list, or None for this machine.
    pub host: Option<usize>,
    /// Empty when the sidebar is showing only this machine's agents.
    pub label: String,
    pub status: Option<String>,
    pub folders: Vec<Folder>,
}

pub struct Folder {
    pub label: String,
    pub cwd: String,
    /// Position in the whole sidebar, 1-based — what you type.
    pub number: usize,
    pub collapsed: bool,
    pub items: Vec<Item>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Item {
    /// Index into the dashboard's agents.
    Live(usize),
    /// Host index, then index into that host's ghosts.
    Ghost(usize, usize),
}

/// One sidebar line.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Row {
    Host(usize),
    /// Section index, folder index within it.
    Folder(usize, usize),
    /// Section, folder, item within the folder.
    Item(usize, usize, usize),
    NewAgent,
}

/// A working directory's own name: the last component, `~` for the home
/// directory itself, and never a path.
fn folder_label(cwd: &str) -> String {
    if cwd.is_empty() {
        return "…".to_string(); // meta hasn't landed yet
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() && cwd == home {
            return "~".to_string();
        }
    }
    match cwd.trim_end_matches('/').rsplit('/').next() {
        Some(name) if !name.is_empty() => name.to_string(),
        _ => "/".to_string(),
    }
}

/// Group agents into the sidebar's sections and folders.
///
/// `entries` must already be in sidebar order (see the dashboard's
/// `sort_agents`), which is what makes each folder a run rather than a search.
pub fn build(
    entries: &[Entry],
    hosts: &[HostView],
    collapsed: &HashSet<String>,
    local_label: &str,
) -> Vec<Section> {
    // This machine first, then the hosts in the order the file lists them.
    let mut sections: Vec<Section> = Vec::with_capacity(hosts.len() + 1);
    let named = !hosts.is_empty(); // a lone machine needs no heading
    sections.push(Section {
        host: None,
        label: if named { local_label.to_string() } else { String::new() },
        status: None,
        folders: Vec::new(),
    });
    for (i, host) in hosts.iter().enumerate() {
        sections.push(Section {
            host: Some(i),
            label: host.label.to_string(),
            status: host.status.clone(),
            folders: Vec::new(),
        });
    }

    let section_of = |host: Option<&str>| -> usize {
        match host {
            None => 0,
            Some(dest) => hosts.iter().position(|h| h.dest == dest).map(|i| i + 1).unwrap_or(0),
        }
    };

    for entry in entries {
        let s = section_of(entry.host);
        push_item(&mut sections[s].folders, entry.cwd, Item::Live(entry.index), collapsed);
    }
    // A machine that isn't answering shows what it had, greyed.
    for (i, host) in hosts.iter().enumerate() {
        if host.status.is_none() {
            continue;
        }
        for (g, ghost) in host.ghosts.iter().enumerate() {
            push_item(&mut sections[i + 1].folders, &ghost.cwd, Item::Ghost(i, g), collapsed);
        }
    }

    // Folder numbers run through the whole sidebar, so the two digits you
    // type never depend on which machine a folder sits under.
    let mut number = 0;
    for section in &mut sections {
        for folder in &mut section.folders {
            number += 1;
            folder.number = number;
        }
    }
    sections
}

fn push_item(folders: &mut Vec<Folder>, cwd: &str, item: Item, collapsed: &HashSet<String>) {
    match folders.last_mut() {
        Some(f) if f.cwd == cwd => f.items.push(item),
        _ => folders.push(Folder {
            label: folder_label(cwd),
            cwd: cwd.to_string(),
            number: 0,
            collapsed: collapsed.contains(cwd),
            items: vec![item],
        }),
    }
}

/// Labels that would read the same take one parent component to tell them
/// apart — `phylo/src/`, not the path that got you there. Applied across the
/// whole sidebar, so two machines with a `src/` each are still distinct.
pub fn disambiguate(sections: &mut [Section]) {
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for section in sections.iter() {
        for folder in &section.folders {
            *seen.entry(folder.label.clone()).or_insert(0) += 1;
        }
    }
    for section in sections.iter_mut() {
        for folder in &mut section.folders {
            if seen.get(&folder.label).copied().unwrap_or(0) < 2 {
                continue;
            }
            let trimmed = folder.cwd.trim_end_matches('/');
            let above = trimmed[..trimmed.len() - folder.label.len().min(trimmed.len())]
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or("");
            if !above.is_empty() {
                folder.label = format!("{above}/{}", folder.label);
            }
        }
    }
}

/// Flatten to lines: a heading per machine (when there is more than one), the
/// folders under it, the agents of the folders that are open, and the + tab.
pub fn rows(sections: &[Section]) -> Vec<Row> {
    let mut rows = Vec::new();
    for (s, section) in sections.iter().enumerate() {
        if section.folders.is_empty() && section.status.is_none() {
            continue; // a machine with nothing on it, and nothing to say
        }
        if !section.label.is_empty() {
            rows.push(Row::Host(s));
        }
        for (f, folder) in section.folders.iter().enumerate() {
            rows.push(Row::Folder(s, f));
            if !folder.collapsed {
                rows.extend((0..folder.items.len()).map(|i| Row::Item(s, f, i)));
            }
        }
    }
    rows.push(Row::NewAgent);
    rows
}

/// The agent a folder number and an in-folder position name, if it is one you
/// can actually go to.
pub fn locate(sections: &[Section], folder: usize, item: usize) -> Result<usize, String> {
    let Some((section, f)) = sections
        .iter()
        .flat_map(|s| s.folders.iter().map(move |f| (s, f)))
        .find(|(_, f)| f.number == folder)
    else {
        return Err(format!("no folder {folder}"));
    };
    let Some(found) = item.checked_sub(1).and_then(|i| f.items.get(i)) else {
        return Err(format!("{}/ has no agent {item}", f.label));
    };
    match found {
        Item::Live(index) => Ok(*index),
        Item::Ghost(..) => Err(format!(
            "{} is not answering",
            if section.label.is_empty() { "that machine" } else { &section.label }
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ghost(name: &str, cwd: &str) -> crate::remote::Ghost {
        crate::remote::Ghost {
            name: name.into(),
            display: name.into(),
            cwd: cwd.into(),
            slot: 1,
            created: 0,
        }
    }

    fn shape(sections: &[Section]) -> Vec<String> {
        rows(sections)
            .iter()
            .map(|r| match r {
                Row::Host(s) => format!("[{}]", sections[*s].label),
                Row::Folder(s, f) => {
                    let folder = &sections[*s].folders[*f];
                    format!("{} {}/", folder.number, folder.label)
                }
                Row::Item(s, f, i) => match sections[*s].folders[*f].items[*i] {
                    Item::Live(n) => format!("  live {n}"),
                    Item::Ghost(h, g) => format!("  ghost {h}.{g}"),
                },
                Row::NewAgent => "+".to_string(),
            })
            .collect()
    }

    #[test]
    fn one_machine_needs_no_heading() {
        let entries = vec![
            Entry { index: 0, host: None, cwd: "/w/research" },
            Entry { index: 1, host: None, cwd: "/w/research" },
            Entry { index: 2, host: None, cwd: "/w/phylo" },
        ];
        let sections = build(&entries, &[], &HashSet::new(), "here");
        assert_eq!(
            shape(&sections),
            ["1 research/", "  live 0", "  live 1", "2 phylo/", "  live 2", "+"]
        );
    }

    #[test]
    fn folder_numbers_run_through_the_machines() {
        let hosts = [HostView { dest: "smq", label: "smq", status: None, ghosts: &[] }];
        let entries = vec![
            Entry { index: 0, host: None, cwd: "/w/research" },
            Entry { index: 1, host: None, cwd: "/w/phylo" },
            Entry { index: 2, host: Some("smq"), cwd: "/r/games" },
            Entry { index: 3, host: Some("smq"), cwd: "/r/mods" },
        ];
        let sections = build(&entries, &hosts, &HashSet::new(), "here");
        assert_eq!(
            shape(&sections),
            [
                "[here]", "1 research/", "  live 0", "2 phylo/", "  live 1",
                "[smq]", "3 games/", "  live 2", "4 mods/", "  live 3", "+",
            ]
        );
        // Folders 3 and 4 are the remote ones, and two digits still reach them.
        assert_eq!(locate(&sections, 3, 1), Ok(2));
        assert_eq!(locate(&sections, 4, 1), Ok(3));
        assert!(locate(&sections, 5, 1).is_err());
    }

    #[test]
    fn an_unanswering_machine_keeps_its_rows_but_they_are_not_targets() {
        let ghosts = [ghost("spork", "/r/games"), ghost("terrain", "/r/games")];
        let hosts = [HostView {
            dest: "smq",
            label: "smq",
            status: Some("reconnecting…".into()),
            ghosts: &ghosts,
        }];
        let entries = vec![Entry { index: 0, host: None, cwd: "/w/research" }];
        let sections = build(&entries, &hosts, &HashSet::new(), "here");
        assert_eq!(
            shape(&sections),
            ["[here]", "1 research/", "  live 0", "[smq]", "2 games/", "  ghost 0.0", "  ghost 0.1", "+"]
        );
        assert_eq!(locate(&sections, 2, 1), Err("smq is not answering".into()));
        assert_eq!(locate(&sections, 1, 1), Ok(0));
    }

    #[test]
    fn same_named_directories_on_two_machines_stay_distinct() {
        let hosts = [HostView { dest: "smq", label: "smq", status: None, ghosts: &[] }];
        let entries = vec![
            Entry { index: 0, host: None, cwd: "/w/phylo/src" },
            Entry { index: 1, host: Some("smq"), cwd: "/r/warren/src" },
        ];
        let mut sections = build(&entries, &hosts, &HashSet::new(), "here");
        disambiguate(&mut sections);
        assert_eq!(sections[0].folders[0].label, "phylo/src");
        assert_eq!(sections[1].folders[0].label, "warren/src");
    }

    #[test]
    fn folding_hides_agents_and_a_quiet_machine_disappears() {
        let hosts = [HostView { dest: "smq", label: "smq", status: None, ghosts: &[] }];
        let entries = vec![Entry { index: 0, host: None, cwd: "/w/a" }];
        let collapsed: HashSet<String> = ["/w/a".to_string()].into_iter().collect();
        let sections = build(&entries, &hosts, &collapsed, "here");
        // smq is answering and has nothing running: no heading for it.
        assert_eq!(shape(&sections), ["[here]", "1 a/", "+"]);
    }
}
