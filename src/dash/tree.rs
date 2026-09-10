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
    /// Home directory on the machine this agent is on — what makes a folder
    /// a project rather than a path. Empty if that machine hasn't said.
    pub home: &'a str,
}

/// One machine, as the tree needs it.
pub struct HostView<'a> {
    pub dest: &'a str,
    pub label: &'a str,
    /// That machine's home directory, if it reported one.
    pub home: &'a str,
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
    /// The same path as the machine it is on reads it (`~/Developer`), which
    /// is what says whether two folders are the same place or merely read
    /// the same.
    pub rel: String,
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

/// The folder a working directory belongs to: under home, the first thing
/// *inside* home — so every checkout under `~/Developer` is one folder, not
/// one folder each. Home itself is its own folder, and a directory somewhere
/// else on the machine is still just itself.
///
/// Always a prefix of `cwd`, so this costs nothing to compute: the sidebar
/// works it out for every agent on every frame.
pub fn folder_key<'a>(cwd: &'a str, home: &str) -> &'a str {
    let cwd = cwd.trim_end_matches('/');
    let home = home.trim_end_matches('/');
    if home.is_empty() || cwd == home {
        return cwd;
    }
    let Some(rest) = cwd.strip_prefix(home).and_then(|r| r.strip_prefix('/')) else {
        return cwd; // outside home entirely
    };
    match rest.find('/') {
        Some(i) => &cwd[..home.len() + 1 + i],
        None => cwd, // already directly inside home
    }
}

/// Where home probably is, for a machine that has not said: the first two
/// components of a `/Users` or `/home` path. Only ever used to group rows,
/// and only for a machine running a warren too old to report its own home.
fn guess_home(cwd: &str) -> &str {
    let mut parts = cwd.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(""), Some(base @ ("Users" | "home")), Some(user)) if !user.is_empty() => {
            &cwd[..1 + base.len() + 1 + user.len()]
        }
        _ => "",
    }
}

fn home_or_guess<'a>(cwd: &'a str, home: &'a str) -> &'a str {
    if home.is_empty() { guess_home(cwd) } else { home }
}

/// A folder's own name: the last component of its path, `~` for home itself,
/// and never a path.
/// A folder's path as its own machine reads it: `~/Developer` here is
/// `~/Developer` over there too, whoever's home it happens to be.
fn relative_to_home(key: &str, home: &str) -> String {
    let home = home.trim_end_matches('/');
    if home.is_empty() {
        return key.to_string();
    }
    match key.strip_prefix(home) {
        Some("") => "~".to_string(),
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => key.to_string(),
    }
}

fn folder_label(key: &str, home: &str) -> String {
    if key.is_empty() {
        return "…".to_string(); // meta hasn't landed yet
    }
    let home = home.trim_end_matches('/');
    if !home.is_empty() && key == home {
        return "~".to_string();
    }
    match key.rsplit('/').next() {
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
        let home = home_or_guess(entry.cwd, entry.home);
        push_item(&mut sections[s].folders, entry.cwd, home, Item::Live(entry.index), collapsed);
    }
    // A machine that isn't answering shows what it had, greyed.
    for (i, host) in hosts.iter().enumerate() {
        if host.status.is_none() {
            continue;
        }
        for (g, ghost) in host.ghosts.iter().enumerate() {
            let home = home_or_guess(&ghost.cwd, host.home);
            push_item(&mut sections[i + 1].folders, &ghost.cwd, home, Item::Ghost(i, g), collapsed);
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

fn push_item(
    folders: &mut Vec<Folder>,
    cwd: &str,
    home: &str,
    item: Item,
    collapsed: &HashSet<String>,
) {
    let key = folder_key(cwd, home);
    match folders.last_mut() {
        Some(f) if f.cwd == key => f.items.push(item),
        _ => folders.push(Folder {
            label: folder_label(key, home),
            cwd: key.to_string(),
            rel: relative_to_home(key, home),
            number: 0,
            collapsed: collapsed.contains(key),
            items: vec![item],
        }),
    }
}

/// Labels that would read the same take one parent component to tell them
/// apart — `phylo/src/`, not the path that got you there. Applied across the
/// whole sidebar, so two machines with a different `src/` each are still
/// distinct — but the same place on two machines is not two places, and
/// `~/Developer` is `~/Developer` whoever's home it is: spelling out whose
/// would say nothing the heading above the row has not already said.
pub fn disambiguate(sections: &mut [Section]) {
    let mut places: std::collections::HashMap<&str, HashSet<&str>> =
        std::collections::HashMap::new();
    for section in sections.iter() {
        for folder in &section.folders {
            places.entry(folder.label.as_str()).or_default().insert(folder.rel.as_str());
        }
    }
    let ambiguous: HashSet<String> = places
        .iter()
        .filter(|(_, places)| places.len() > 1)
        .map(|(label, _)| (*label).to_string())
        .collect();
    for section in sections.iter_mut() {
        for folder in &mut section.folders {
            if !ambiguous.contains(&folder.label) {
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
            Entry { index: 0, host: None, cwd: "/w/research", home: "" },
            Entry { index: 1, host: None, cwd: "/w/research", home: "" },
            Entry { index: 2, host: None, cwd: "/w/phylo", home: "" },
        ];
        let sections = build(&entries, &[], &HashSet::new(), "here");
        assert_eq!(
            shape(&sections),
            ["1 research/", "  live 0", "  live 1", "2 phylo/", "  live 2", "+"]
        );
    }

    #[test]
    fn folder_numbers_run_through_the_machines() {
        let hosts = [HostView { dest: "smq", label: "smq", home: "", status: None, ghosts: &[] }];
        let entries = vec![
            Entry { index: 0, host: None, cwd: "/w/research", home: "" },
            Entry { index: 1, host: None, cwd: "/w/phylo", home: "" },
            Entry { index: 2, host: Some("smq"), cwd: "/r/games", home: "" },
            Entry { index: 3, host: Some("smq"), cwd: "/r/mods", home: "" },
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
            home: "",
            status: Some("reconnecting…".into()),
            ghosts: &ghosts,
        }];
        let entries = vec![Entry { index: 0, host: None, cwd: "/w/research", home: "" }];
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
        let hosts = [HostView { dest: "smq", label: "smq", home: "", status: None, ghosts: &[] }];
        let entries = vec![
            Entry { index: 0, host: None, cwd: "/w/phylo/src", home: "" },
            Entry { index: 1, host: Some("smq"), cwd: "/r/warren/src", home: "" },
        ];
        let mut sections = build(&entries, &hosts, &HashSet::new(), "here");
        disambiguate(&mut sections);
        assert_eq!(sections[0].folders[0].label, "phylo/src");
        assert_eq!(sections[1].folders[0].label, "warren/src");
    }

    /// A folder is a place you keep projects, not a project: everything
    /// under ~/Developer is one row, however many checkouts deep it goes.
    #[test]
    fn folders_are_the_things_directly_inside_home() {
        let home = "/Users/sam";
        let entries = vec![
            Entry { index: 0, host: None, cwd: "/Users/sam/Developer/warren", home },
            Entry { index: 1, host: None, cwd: "/Users/sam/Developer/Phylogen", home },
            Entry { index: 2, host: None, cwd: "/Users/sam/Developer/plugins/dsp/src", home },
            Entry { index: 3, host: None, cwd: "/Users/sam/Research", home },
            // Home itself, and somewhere else on the machine entirely.
            Entry { index: 4, host: None, cwd: "/Users/sam", home },
            Entry { index: 5, host: None, cwd: "/private/tmp", home },
        ];
        let sections = build(&entries, &[], &HashSet::new(), "here");
        assert_eq!(
            shape(&sections),
            [
                "1 Developer/",
                "  live 0",
                "  live 1",
                "  live 2",
                "2 Research/",
                "  live 3",
                "3 ~/",
                "  live 4",
                "4 tmp/",
                "  live 5",
                "+",
            ]
        );
        // The folder is the directory, so folding it folds all three.
        assert_eq!(sections[0].folders[0].cwd, "/Users/sam/Developer");
    }

    #[test]
    fn a_home_no_one_reported_is_guessed_from_the_path() {
        // An older warren over there says no home; the paths still do.
        let entries = vec![
            Entry { index: 0, host: Some("smq"), cwd: "/Users/critter/Developer/warren", home: "" },
            Entry { index: 1, host: Some("smq"), cwd: "/home/critter/Games/spork", home: "" },
            // Nothing to guess from: the directory is its own folder.
            Entry { index: 2, host: Some("smq"), cwd: "/opt/pmk/env", home: "" },
        ];
        let hosts = [HostView { dest: "smq", label: "smq", home: "", status: None, ghosts: &[] }];
        let sections = build(&entries, &hosts, &HashSet::new(), "here");
        assert_eq!(
            shape(&sections),
            ["[smq]", "1 Developer/", "  live 0", "2 Games/", "  live 1", "3 env/", "  live 2", "+"]
        );
    }

    /// The same place on two machines is one place said twice, and the
    /// heading already says which is which — but two different `src/`
    /// still have to be told apart.
    #[test]
    fn the_same_folder_on_two_machines_is_not_two_folders() {
        let entries = vec![
            Entry { index: 0, host: None, cwd: "/Users/sam/Developer/warren", home: "/Users/sam" },
            Entry {
                index: 1,
                host: Some("smq"),
                cwd: "/Users/critter/Developer/warren",
                home: "/Users/critter",
            },
        ];
        let hosts =
            [HostView { dest: "smq", label: "smq", home: "/Users/critter", status: None, ghosts: &[] }];
        let mut sections = build(&entries, &hosts, &HashSet::new(), "here");
        disambiguate(&mut sections);
        assert_eq!(
            shape(&sections),
            ["[here]", "1 Developer/", "  live 0", "[smq]", "2 Developer/", "  live 1", "+"]
        );
    }

    #[test]
    fn folding_hides_agents_and_a_quiet_machine_disappears() {
        let hosts = [HostView { dest: "smq", label: "smq", home: "", status: None, ghosts: &[] }];
        let entries = vec![Entry { index: 0, host: None, cwd: "/w/a", home: "" }];
        let collapsed: HashSet<String> = ["/w/a".to_string()].into_iter().collect();
        let sections = build(&entries, &hosts, &collapsed, "here");
        // smq is answering and has nothing running: no heading for it.
        assert_eq!(shape(&sections), ["[here]", "1 a/", "+"]);
    }
}
