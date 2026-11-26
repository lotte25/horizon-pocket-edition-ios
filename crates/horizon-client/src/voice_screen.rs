//! Voice chat's page of the pause menu's Friends screen in a shared world (`game/voice.rs`
//! fills it). Top to bottom: voice chat on or off beside the microphone on or muted; which
//! microphone beside everyone's volume together; then each of the others, a slider for their
//! volume (0 to 200 %) beside Mute, a page of them at a time; and at the foot the noise filter
//! of this player's own microphone beside Back.
//!
//! A slider takes a tap as well as a drag: on a phone a finger's tap sets it.
use crate::{
    hud::{HudAtlas, HudBatch, rgba},
    multiplayer::wrap,
    preedit,
    text::Text,
    ui,
};
use horizon_voice::svc::GroupType;

/// The English title.
pub const TITLE: &str = "Voice Chat";

fn tr(key: &str, english: &str) -> String {
    crate::lang::get_or(&format!("horizon.voice.{key}"), english)
}

/// The loudest a volume goes: 200 %.
pub const MAX_VOLUME: f32 = 2.0;

/// Which page of Voice Chat is shown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    People,
    Groups,
}

/// Which text field in the Groups tab has keyboard focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupField {
    Name,
    Password,
}

/// One voice group on a Simple Voice Chat server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupView {
    pub id: u128,
    pub name: String,
    pub has_password: bool,
    pub kind: GroupType,
    pub members: usize,
    pub joined: bool,
}

/// One of the others.
#[derive(Clone, Debug, PartialEq)]
pub struct Person {
    pub uuid: u128,
    pub name: String,
    /// Voice chat is on in their game, and their microphone is.
    pub enabled: bool,
    pub mic_live: bool,
    /// How this player hears them: 0 to [`MAX_VOLUME`], and not at all.
    pub volume: f32,
    pub muted: bool,
    pub talking: bool,
}

/// What the page shows.
#[derive(Clone, Debug, Default)]
pub struct View {
    /// The host's game carries voices (else nothing here can be turned on).
    pub offered: bool,
    pub enabled: bool,
    pub mic_live: bool,
    /// How loud the microphone hears now (0 to 1), and whether speech is going out.
    pub mic_level: f32,
    pub speaking: bool,
    /// The microphone in use, as the player reads it ("Automatic", "iPhone Microphone"), and
    /// whether there is another to choose.
    pub mic_name: String,
    pub mic_choice: bool,
    /// Everyone's volume together, 0 to [`MAX_VOLUME`].
    pub voices: f32,
    /// The noise filter on this player's own microphone.
    pub noise_filter: bool,
    pub people: Vec<Person>,
    /// The first of the people shown.
    pub first: usize,
    /// A line under the settings: why something can't be done, or what was just done.
    pub note: Option<String>,
    /// Simple Voice Chat groups are enabled on this server.
    pub groups_enabled: bool,
    /// Current tab (`People` or `Groups`).
    pub tab: Tab,
    /// Known groups on the server.
    pub groups: Vec<GroupView>,
    /// Group the local player is currently in.
    pub joined_group: Option<u128>,
    /// The Create Group form is open inside the Groups tab.
    pub create_mode: bool,
    /// Joining a password-protected group: `(group_id, group_name)`.
    pub join_password_for: Option<(u128, String)>,
    /// Text in the Group Name field.
    pub name_input: String,
    /// Text in the Group Password field.
    pub password_input: String,
    /// Selected group type in the Create Group form.
    pub group_type: GroupType,
    /// Currently focused text field, if any.
    pub focused_field: Option<GroupField>,
    /// First group index shown on the Groups tab.
    pub groups_first: usize,
}

/// What a tap or a drag asks for.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Enable(bool),
    Mic(bool),
    /// The next microphone of the device's.
    NextMic,
    /// The noise filter on this player's own microphone, on or off.
    NoiseFilter(bool),
    Voices(f32),
    Volume(u128, f32),
    Mute(u128, bool),
    /// Show the people from this one on.
    Page(usize),
    /// Switch between the Players and Groups tabs.
    Tab(Tab),
    /// Join a group (or prompt for password if locked).
    JoinGroup(u128),
    /// Leave the current group.
    LeaveGroup,
    /// Open the Create Group form.
    OpenCreateGroup,
    /// Close Create Group or Join Password form back to the group list.
    CancelGroupForm,
    /// Cycle group type (Normal -> Open -> Isolated).
    CycleGroupType,
    /// Submit the Create Group form.
    SubmitCreateGroup,
    /// Submit the Join Password prompt.
    SubmitJoinPassword,
    /// Focus a text field inside the Groups tab.
    FocusField(GroupField),
    /// Show the groups from this index on.
    GroupPage(usize),
    Back,
}

/// A slider being dragged: whose, and where it sits.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Held {
    /// None for everyone's volume together.
    pub person: Option<u128>,
    pub x: f32,
    pub w: f32,
}

impl Held {
    /// What the slider asks for with the cursor at `cx`.
    pub fn action(&self, cx: f32) -> Action {
        let volume = volume_at(self.x, self.w, cx);
        match self.person {
            Some(uuid) => Action::Volume(uuid, volume),
            None => Action::Voices(volume),
        }
    }
}

/// The volume a slider at `x`, `w` wide, shows under a cursor at `cx`: in steps of 5 %.
pub fn volume_at(x: f32, w: f32, cx: f32) -> f32 {
    let steps = (ui::slider_value(x, w, cx) * 40.0).round();
    steps / 40.0 * MAX_VOLUME
}

fn percent(volume: f32) -> String {
    format!("{}%", (volume * 100.0).round() as i32)
}

const WHITE: u32 = u32::MAX;
fn grey() -> u32 {
    rgba(170, 170, 170, 255)
}
fn soft() -> u32 {
    rgba(255, 255, 160, 255)
}
fn green() -> u32 {
    rgba(85, 255, 85, 255)
}

enum Kind {
    Button(Action),
    /// A slider at `value` (0 to 1): a person's volume, or everyone's (None).
    Slider {
        person: Option<u128>,
        value: f32,
    },
    /// A single-line text input box (group name or password).
    Field {
        field: GroupField,
        value: String,
        placeholder: String,
        focused: bool,
        password: bool,
    },
}

struct Item {
    x: f32,
    y: f32,
    w: f32,
    label: String,
    active: bool,
    kind: Kind,
    /// A dot before it: the person is talking (or the group is currently joined).
    talking: bool,
}

/// Rows of people or groups that fit between the settings and the footer.
fn rows(h: f32, top: f32) -> usize {
    (((h - 33.0 - top) / 24.0).floor() as usize).max(1)
}

/// Truncates `text` with `..` so its rendered width fits inside `max_w`.
fn fit_text(atlas: &HudAtlas, text: &str, max_w: f32) -> String {
    if HudBatch::text_width(atlas, text) <= max_w {
        return text.to_owned();
    }
    let mut out = String::new();
    for ch in text.chars() {
        let mut cand = out.clone();
        cand.push(ch);
        cand.push_str("..");
        if HudBatch::text_width(atlas, &cand) > max_w {
            break;
        }
        out.push(ch);
    }
    out.push_str("..");
    out
}

fn group_type_name(kind: GroupType) -> String {
    match kind {
        GroupType::Normal => tr("group_type_normal", "Normal"),
        GroupType::Open => tr("group_type_open", "Open"),
        GroupType::Isolated => tr("group_type_isolated", "Isolated"),
    }
}

fn group_type_desc(kind: GroupType) -> String {
    match kind {
        GroupType::Normal => tr(
            "group_desc_normal",
            "Normal: hear group members and nearby players.",
        ),
        GroupType::Open => tr("group_desc_open", "Open: nearby players can also hear you."),
        GroupType::Isolated => tr(
            "group_desc_isolated",
            "Isolated: only hear members of this group.",
        ),
    }
}

fn layout_groups(
    atlas: &HudAtlas,
    (w, h): (f32, f32),
    view: &View,
) -> (Vec<(String, u32, f32)>, Vec<Item>) {
    let cx = (w / 2.0).floor();
    let (left, right, half) = (cx - 152.0, cx + 2.0, 150.0);
    let full = 304.0;
    let mut lines = Vec::new();
    let mut items = Vec::new();
    let button = |x: f32, y: f32, w: f32, label: String, active: bool, action: Action| Item {
        x,
        y,
        w,
        label,
        active,
        kind: Kind::Button(action),
        talking: false,
    };

    if view.create_mode {
        let heading = match &view.note {
            Some(note) => (note.clone(), soft()),
            None => (tr("create_group_heading", "Create a voice group"), grey()),
        };
        let mut y: f32 = 32.0;
        for line in wrap(&heading.0, (w - 40.0).min(304.0), atlas)
            .into_iter()
            .take(1)
        {
            lines.push((line, heading.1, y));
            y += 12.0;
        }
        y = y.max(46.0);
        items.push(Item {
            x: left,
            y,
            w: full,
            label: String::new(),
            active: true,
            kind: Kind::Field {
                field: GroupField::Name,
                value: view.name_input.clone(),
                placeholder: tr("group_name_placeholder", "Group Name"),
                focused: view.focused_field == Some(GroupField::Name),
                password: false,
            },
            talking: false,
        });
        y += 26.0;
        items.push(Item {
            x: left,
            y,
            w: full,
            label: String::new(),
            active: true,
            kind: Kind::Field {
                field: GroupField::Password,
                value: view.password_input.clone(),
                placeholder: tr("group_password_optional", "Password (optional)"),
                focused: view.focused_field == Some(GroupField::Password),
                password: true,
            },
            talking: false,
        });
        y += 26.0;
        items.push(button(
            left,
            y,
            full,
            tr("group_type_button", "Type: %s").replacen(
                "%s",
                &group_type_name(view.group_type),
                1,
            ),
            true,
            Action::CycleGroupType,
        ));
        y += 24.0;
        let desc = group_type_desc(view.group_type);
        for line in wrap(&desc, (w - 40.0).min(304.0), atlas)
            .into_iter()
            .take(2)
        {
            lines.push((line, grey(), y));
            y += 10.0;
        }
        y += 4.0;
        let can_create = view.enabled && !view.name_input.trim().is_empty();
        items.push(button(
            left,
            y,
            half,
            tr("create", "Create"),
            can_create,
            Action::SubmitCreateGroup,
        ));
        items.push(button(
            right,
            y,
            half,
            tr("cancel", "Cancel"),
            true,
            Action::CancelGroupForm,
        ));
    } else if let Some((_, ref group_name)) = view.join_password_for {
        let mut y = 34.0;
        let prompt =
            tr("group_password_prompt", "Enter password for %s").replacen("%s", group_name, 1);
        for line in wrap(&prompt, (w - 40.0).min(304.0), atlas)
            .into_iter()
            .take(2)
        {
            lines.push((line, WHITE, y));
            y += 10.0;
        }
        if let Some(note) = &view.note {
            y += 2.0;
            for line in wrap(note, (w - 40.0).min(304.0), atlas).into_iter().take(2) {
                lines.push((line, soft(), y));
                y += 10.0;
            }
        }
        y += 6.0;
        items.push(Item {
            x: left,
            y,
            w: full,
            label: String::new(),
            active: true,
            kind: Kind::Field {
                field: GroupField::Password,
                value: view.password_input.clone(),
                placeholder: tr("group_password_placeholder", "Password"),
                focused: view.focused_field == Some(GroupField::Password),
                password: true,
            },
            talking: false,
        });
        y += 28.0;
        let can_join = view.enabled && !view.password_input.is_empty();
        items.push(button(
            left,
            y,
            half,
            tr("join", "Join"),
            can_join,
            Action::SubmitJoinPassword,
        ));
        items.push(button(
            right,
            y,
            half,
            tr("cancel", "Cancel"),
            true,
            Action::CancelGroupForm,
        ));
    } else {
        // Group list view.
        items.push(button(
            left,
            32.0,
            half,
            tr("create_group", "Create Group"),
            view.enabled,
            Action::OpenCreateGroup,
        ));
        items.push(button(
            right,
            32.0,
            half,
            tr("leave_group", "Leave Group"),
            view.joined_group.is_some(),
            Action::LeaveGroup,
        ));
        let note = match &view.note {
            Some(note) => Some((note.clone(), soft())),
            None if !view.enabled => Some((
                tr(
                    "groups_need_voice",
                    "Turn Voice Chat on to join or create groups.",
                ),
                grey(),
            )),
            None if view.groups.is_empty() => Some((
                tr(
                    "no_groups",
                    "No voice groups yet. Create one to talk privately.",
                ),
                grey(),
            )),
            None => view.groups.iter().find(|g| g.joined).map(|g| {
                (
                    tr("in_group", "In group: %s (%t)")
                        .replacen("%s", &g.name, 1)
                        .replacen("%t", &group_type_name(g.kind), 1),
                    green(),
                )
            }),
        };
        let mut y = 58.0;
        if let Some((note, color)) = note {
            for line in wrap(&note, (w - 40.0).min(304.0), atlas)
                .into_iter()
                .take(2)
            {
                lines.push((line, color, y));
                y += 10.0;
            }
            y += 2.0;
        }
        let n = rows(h, y);
        let first = if view.groups.is_empty() {
            0
        } else {
            view.groups_first.min(view.groups.len() - 1) / n * n
        };
        for (i, group) in view.groups.iter().skip(first).take(n).enumerate() {
            let row = y + 24.0 * i as f32;
            let lock = if group.has_password { "[*] " } else { "" };
            let raw = format!(
                "{lock}{} ({}, {})",
                group.name,
                group_type_name(group.kind),
                group.members
            );
            let label = fit_text(atlas, &raw, 200.0);
            items.push(Item {
                x: left + 10.0,
                y: row,
                w: 208.0,
                label,
                active: view.enabled && !group.joined,
                kind: Kind::Button(Action::JoinGroup(group.id)),
                talking: group.joined,
            });
            items.push(button(
                left + 222.0,
                row,
                82.0,
                if group.joined {
                    tr("leave", "Leave")
                } else {
                    tr("join", "Join")
                },
                group.joined || view.enabled,
                if group.joined {
                    Action::LeaveGroup
                } else {
                    Action::JoinGroup(group.id)
                },
            ));
        }
        if view.groups.len() > n {
            items.push(button(
                left,
                h - 27.0,
                20.0,
                "<".into(),
                first > 0,
                Action::GroupPage(first.saturating_sub(n)),
            ));
            items.push(button(
                left + 22.0,
                h - 27.0,
                20.0,
                ">".into(),
                first + n < view.groups.len(),
                Action::GroupPage(first + n),
            ));
        }
    }

    // Footer: Players tab button + Back (always last).
    items.push(button(
        cx - 90.0,
        h - 27.0,
        86.0,
        tr("players_tab", "Players"),
        true,
        Action::Tab(Tab::People),
    ));
    items.push(button(
        cx + 4.0,
        h - 27.0,
        86.0,
        "gui.back".into(),
        true,
        Action::Back,
    ));
    (lines, items)
}

fn layout(
    atlas: &HudAtlas,
    (w, h): (f32, f32),
    view: &View,
) -> (Vec<(String, u32, f32)>, Vec<Item>) {
    if view.groups_enabled && view.tab == Tab::Groups {
        return layout_groups(atlas, (w, h), view);
    }
    let cx = (w / 2.0).floor();
    let (left, right, half) = (cx - 152.0, cx + 2.0, 150.0);
    let mut lines = Vec::new();
    let mut items = Vec::new();
    let button = |x: f32, y: f32, w: f32, label: String, active: bool, action: Action| Item {
        x,
        y,
        w,
        label,
        active,
        kind: Kind::Button(action),
        talking: false,
    };
    // On or off, and the microphone.
    items.push(button(
        left,
        32.0,
        half,
        if view.enabled {
            tr("on_button", "Voice Chat: On")
        } else {
            tr("off_button", "Voice Chat: Off")
        },
        view.offered,
        Action::Enable(!view.enabled),
    ));
    items.push(button(
        right,
        32.0,
        half,
        if view.mic_live {
            tr("mic_on", "Microphone: On")
        } else {
            tr("mic_muted", "Microphone: Muted")
        },
        view.enabled,
        Action::Mic(!view.mic_live),
    ));
    // Which microphone, and everyone's volume together.
    items.push(button(
        left,
        56.0,
        half,
        tr("mic_named", "Mic: %s").replacen("%s", &view.mic_name, 1),
        view.mic_choice,
        Action::NextMic,
    ));
    items.push(Item {
        x: right,
        y: 56.0,
        w: half,
        label: tr("voices", "Voices: %s").replacen("%s", &percent(view.voices), 1),
        active: view.enabled,
        kind: Kind::Slider {
            person: None,
            value: view.voices / MAX_VOLUME,
        },
        talking: false,
    });
    // One line of what is going on.
    let note = match &view.note {
        Some(note) => Some((note.clone(), soft())),
        None if !view.offered => Some((
            tr(
                "not_offered",
                "The host's game doesn't have voice chat yet.",
            ),
            grey(),
        )),
        None if !view.enabled => Some((
            tr(
                "explain",
                "On, you hear the others. Your microphone stays off until you turn it on.",
            ),
            grey(),
        )),
        None if view.people.is_empty() => {
            Some((tr("nobody", "Nobody else is in voice chat yet."), grey()))
        }
        None => None,
    };
    let mut y = 80.0;
    if let Some((note, color)) = note {
        for line in wrap(&note, (w - 40.0).min(310.0), atlas)
            .into_iter()
            .take(2)
        {
            lines.push((line, color, y));
            y += 10.0;
        }
        y += 2.0;
    }
    // The others, a page of them.
    let n = rows(h, y);
    let first = if view.people.is_empty() {
        0
    } else {
        view.first.min(view.people.len() - 1) / n * n
    };
    for (i, person) in view.people.iter().skip(first).take(n).enumerate() {
        let row = y + 24.0 * i as f32;
        let state = if !person.enabled {
            tr("person_off", " (off)")
        } else if !person.mic_live {
            tr("person_muted", " (mic off)")
        } else {
            String::new()
        };
        items.push(Item {
            x: left + 10.0,
            y: row,
            w: 208.0,
            label: format!("{}{state}: {}", person.name, percent(person.volume)),
            active: view.enabled && !person.muted,
            kind: Kind::Slider {
                person: Some(person.uuid),
                value: person.volume / MAX_VOLUME,
            },
            talking: person.talking && !person.muted,
        });
        items.push(button(
            left + 222.0,
            row,
            82.0,
            if person.muted {
                tr("unmute", "Unmute")
            } else {
                tr("mute", "Mute")
            },
            true,
            Action::Mute(person.uuid, !person.muted),
        ));
    }
    // The foot: the pages of people when there is more than one, optional Groups tab,
    // Noise Filter, and Back.
    if view.groups_enabled {
        if view.people.len() > n {
            items.push(button(
                left,
                h - 27.0,
                20.0,
                "<".into(),
                first > 0,
                Action::Page(first.saturating_sub(n)),
            ));
            items.push(button(
                left + 22.0,
                h - 27.0,
                20.0,
                ">".into(),
                first + n < view.people.len(),
                Action::Page(first + n),
            ));
        }
        items.push(button(
            cx - 106.0,
            h - 27.0,
            60.0,
            tr("groups_tab", "Groups"),
            view.offered,
            Action::Tab(Tab::Groups),
        ));
        items.push(button(
            cx - 42.0,
            h - 27.0,
            116.0,
            if view.noise_filter {
                tr("noise_on", "Noise Filter: On")
            } else {
                tr("noise_off", "Noise Filter: Off")
            },
            view.offered,
            Action::NoiseFilter(!view.noise_filter),
        ));
        items.push(button(
            cx + 78.0,
            h - 27.0,
            72.0,
            "gui.back".into(),
            true,
            Action::Back,
        ));
    } else {
        if view.people.len() > n {
            items.push(button(
                left,
                h - 27.0,
                20.0,
                "<".into(),
                first > 0,
                Action::Page(first.saturating_sub(n)),
            ));
            items.push(button(
                left + 24.0,
                h - 27.0,
                20.0,
                ">".into(),
                first + n < view.people.len(),
                Action::Page(first + n),
            ));
        }
        // The filter's label is the longer: Back, a short word, gives it the room.
        items.push(button(
            cx - 100.0,
            h - 27.0,
            116.0,
            if view.noise_filter {
                tr("noise_on", "Noise Filter: On")
            } else {
                tr("noise_off", "Noise Filter: Off")
            },
            view.offered,
            Action::NoiseFilter(!view.noise_filter),
        ));
        items.push(button(
            cx + 20.0,
            h - 27.0,
            80.0,
            "gui.back".into(),
            true,
            Action::Back,
        ));
    }
    (lines, items)
}

fn label(key: &str) -> Text {
    if key.starts_with("gui.") {
        Text::translate(key, &[])
    } else {
        Text::literal(key)
    }
}

/// Draws the page. `held` is the slider being dragged, if one is.
pub fn draw(
    batch: &mut HudBatch,
    atlas: &HudAtlas,
    cursor: (f32, f32),
    view: &View,
    held: Option<&Held>,
) {
    let (w, h) = (batch.width, batch.height);
    ui::menu_background(batch, atlas, 0.0, 0.0, w, h, true);
    let title = if view.groups_enabled && view.tab == Tab::Groups {
        Text::literal(tr("groups_title", "Voice Chat - Groups"))
    } else {
        Text::literal(tr("title", TITLE))
    };
    let tw = atlas.font.width_text(&title);
    batch.text_styled(atlas, &title, ((w - tw) / 2.0).floor(), 15.0, WHITE, true);
    let (lines, items) = layout(atlas, (w, h), view);
    for (line, color, y) in lines {
        let lw = HudBatch::text_width(atlas, &line);
        batch.text(atlas, &line, ((w - lw) / 2.0).floor(), y, color, true);
    }
    for item in items {
        let hover = item.active && ui::hovered(cursor, item.x, item.y, item.w, 20.0);
        match &item.kind {
            Kind::Button(action) => {
                ui::button(
                    batch,
                    atlas,
                    item.x,
                    item.y,
                    item.w,
                    20.0,
                    &label(&item.label),
                    item.active,
                    hover,
                );
                if item.talking {
                    batch.fill(atlas, item.x - 8.0, item.y + 8.0, 4.0, 4.0, green());
                }
                // The microphone's level, along the foot of its button, green while speech
                // goes out.
                if matches!(action, Action::Mic(_)) && view.mic_live {
                    let level = (view.mic_level * 4.0).clamp(0.0, 1.0);
                    let color = if view.speaking { green() } else { grey() };
                    batch.fill(
                        atlas,
                        item.x + 3.0,
                        item.y + 17.0,
                        ((item.w - 6.0) * level).floor(),
                        1.0,
                        color,
                    );
                }
            }
            Kind::Slider { person, value } => {
                let dragging = held.is_some_and(|held| held.person == *person);
                ui::slider(
                    batch,
                    atlas,
                    item.x,
                    item.y,
                    item.w,
                    &Text::literal(item.label.clone()),
                    value.clamp(0.0, 1.0),
                    hover,
                    item.active,
                    dragging,
                );
                if item.talking {
                    batch.fill(atlas, item.x - 8.0, item.y + 8.0, 4.0, 4.0, green());
                }
            }
            Kind::Field {
                value,
                placeholder,
                focused,
                password,
                ..
            } => {
                let border = if *focused {
                    WHITE
                } else {
                    rgba(160, 160, 160, 255)
                };
                batch.fill(atlas, item.x, item.y, item.w, 20.0, border);
                batch.fill(
                    atlas,
                    item.x + 1.0,
                    item.y + 1.0,
                    item.w - 2.0,
                    18.0,
                    rgba(0, 0, 0, 220),
                );
                let inner_max = (item.w - 14.0).max(20.0);
                let text_x = item.x + 5.0;
                let text_y = item.y + 6.0;
                if value.is_empty() && !focused {
                    let ph = fit_text(atlas, placeholder, inner_max);
                    batch.text(atlas, &ph, text_x, text_y, grey(), true);
                } else {
                    let shown = if *password {
                        "*".repeat(value.chars().count())
                    } else {
                        value.clone()
                    };
                    let visible = fit_text(atlas, &shown, inner_max);
                    batch.text(atlas, &visible, text_x, text_y, WHITE, true);
                    if *focused {
                        let caret_x = text_x + HudBatch::text_width(atlas, &visible) + 1.0;
                        batch.text(atlas, "_", caret_x, text_y, WHITE, true);
                        preedit::report_field([item.x, item.y, item.w, 20.0]);
                        preedit::report_caret(caret_x, text_y, 10.0);
                    }
                }
            }
        }
    }
}

/// What is under a press: a button's action, or a slider taken hold of with the value its
/// place asks for.
pub fn press(
    atlas: &HudAtlas,
    size: (f32, f32),
    cursor: (f32, f32),
    view: &View,
) -> Option<(Action, Option<Held>)> {
    layout(atlas, size, view)
        .1
        .into_iter()
        .find(|item| item.active && ui::hovered(cursor, item.x, item.y, item.w, 20.0))
        .map(|item| match item.kind {
            Kind::Button(action) => (action, None),
            Kind::Slider { person, .. } => {
                let held = Held {
                    person,
                    x: item.x,
                    w: item.w,
                };
                (held.action(cursor.0), Some(held))
            }
            Kind::Field { field, .. } => (Action::FocusField(field), None),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atlas() -> HudAtlas {
        HudAtlas::build(std::path::Path::new("/nonexistent/horizon-test-assets")).unwrap()
    }

    fn person(n: u128, name: &str) -> Person {
        Person {
            uuid: n,
            name: name.into(),
            enabled: true,
            mic_live: true,
            volume: 1.0,
            muted: false,
            talking: n.is_multiple_of(2),
        }
    }

    /// Voice chat on, the microphone live, seven others.
    fn busy() -> View {
        View {
            offered: true,
            enabled: true,
            mic_live: true,
            mic_level: 0.2,
            speaking: true,
            mic_name: "iPhone Microphone".into(),
            mic_choice: true,
            voices: 1.0,
            noise_filter: false,
            people: (0..7).map(|i| person(i, &format!("Player{i}"))).collect(),
            first: 0,
            note: None,
            ..View::default()
        }
    }

    fn group(id: u128, name: &str, has_password: bool, kind: GroupType, joined: bool) -> GroupView {
        GroupView {
            id,
            name: name.into(),
            has_password,
            kind,
            members: 3,
            joined,
        }
    }

    const SIZES: [(f32, f32); 5] = [
        (320.0, 240.0),
        (341.0, 256.0),
        (427.0, 240.0),
        (520.0, 240.0),
        (640.0, 360.0),
    ];

    #[test]
    fn everything_fits_the_screen_and_nothing_overlaps() {
        let atlas = atlas();
        for view in [
            busy(),
            View::default(),
            View {
                offered: true,
                ..View::default()
            },
            View {
                note: Some("Allow the microphone in Settings > Horizon PE > Microphone, then turn it on again here.".into()),
                ..busy()
            },
            View {
                people: vec![
                    person(1, "A_Name_Of_16_Chr"),
                    Person {
                        mic_live: false,
                        volume: 2.0,
                        ..person(2, "WWWWWWWWWWWWWWWW")
                    },
                ],
                ..busy()
            },
            View {
                groups_enabled: true,
                ..busy()
            },
            View {
                groups_enabled: true,
                tab: Tab::Groups,
                joined_group: Some(1),
                groups: vec![
                    group(1, "Squad Alpha", false, GroupType::Normal, true),
                    group(
                        2,
                        "VeryLongVoiceGroupName1234",
                        true,
                        GroupType::Isolated,
                        false,
                    ),
                    group(3, "Open Lobby", false, GroupType::Open, false),
                    group(4, "Team 4", false, GroupType::Normal, false),
                    group(5, "Team 5", true, GroupType::Isolated, false),
                    group(6, "Team 6", false, GroupType::Open, false),
                    group(7, "Team 7", false, GroupType::Normal, false),
                ],
                ..busy()
            },
            View {
                groups_enabled: true,
                tab: Tab::Groups,
                create_mode: true,
                name_input: "My Squad".into(),
                password_input: "secret".into(),
                group_type: GroupType::Isolated,
                focused_field: Some(GroupField::Name),
                ..busy()
            },
            View {
                groups_enabled: true,
                tab: Tab::Groups,
                join_password_for: Some((2, "Locked Squad".into())),
                password_input: "pass".into(),
                focused_field: Some(GroupField::Password),
                note: Some("Wrong password.".into()),
                ..busy()
            },
        ] {
            for size in SIZES {
                let (lines, items) = layout(&atlas, size, &view);
                for item in &items {
                    assert!(
                        item.x >= 0.0 && item.x + item.w <= size.0,
                        "{size:?}: {} is off the screen",
                        item.label
                    );
                    assert!(item.y >= 30.0 && item.y + 20.0 <= size.1 - 7.0);
                    // What isn't in the footer stays above it.
                    if item.y < size.1 - 27.0 {
                        assert!(item.y + 20.0 <= size.1 - 33.0, "{size:?}: {}", item.label);
                    }
                    // A label fits its button or slider.
                    if !item.label.starts_with("gui.") {
                        let width = HudBatch::text_width(&atlas, &item.label);
                        assert!(
                            width <= item.w - 6.0 || item.w <= 20.0,
                            "{size:?}: \"{}\" is {width} wide in {}",
                            item.label,
                            item.w
                        );
                    }
                }
                for (i, a) in items.iter().enumerate() {
                    for b in &items[i + 1..] {
                        let apart = a.x + a.w <= b.x
                            || b.x + b.w <= a.x
                            || a.y + 20.0 <= b.y
                            || b.y + 20.0 <= a.y;
                        assert!(apart, "{size:?}: {} over {}", a.label, b.label);
                    }
                }
                for (_, _, y) in &lines {
                    assert!(
                        items.iter().all(|i| *y + 9.0 <= i.y || *y >= i.y + 20.0),
                        "{size:?}: a line under a button"
                    );
                }
                // Back is always there, last.
                assert!(matches!(
                    items.last().map(|i| &i.kind),
                    Some(Kind::Button(Action::Back))
                ));
            }
        }
    }

    #[test]
    fn the_others_come_a_page_at_a_time() {
        let atlas = atlas();
        let mut view = busy();
        let size = (427.0, 240.0);
        let shown = |view: &View| -> Vec<u128> {
            layout(&atlas, size, view)
                .1
                .iter()
                .filter_map(|i| match i.kind {
                    Kind::Slider {
                        person: Some(uuid), ..
                    } => Some(uuid),
                    _ => None,
                })
                .collect()
        };
        let page = |view: &View, label: &str| {
            layout(&atlas, size, view)
                .1
                .into_iter()
                .find(|i| i.label == label)
                .map(|i| (i.active, i.kind))
        };
        let n = shown(&view).len();
        assert!((2..7).contains(&n), "{n} people on a phone's screen");
        assert_eq!(shown(&view), (0..n as u128).collect::<Vec<_>>());
        // The first page: only forward.
        assert!(matches!(page(&view, "<"), Some((false, _))));
        let Some((true, Kind::Button(Action::Page(next)))) = page(&view, ">") else {
            panic!("no next page");
        };
        assert_eq!(next, n);
        view.first = next;
        assert_eq!(shown(&view)[0], n as u128);
        assert!(matches!(
            page(&view, "<"),
            Some((true, Kind::Button(Action::Page(0))))
        ));
        // Everyone is on some page, once.
        let mut seen = Vec::new();
        let mut first = 0;
        loop {
            view.first = first;
            seen.extend(shown(&view));
            match page(&view, ">") {
                Some((true, Kind::Button(Action::Page(next)))) => first = next,
                _ => break,
            }
        }
        assert_eq!(seen, (0..7).collect::<Vec<u128>>());
        // People left while a later page was shown: the last page there is.
        view.people.truncate(2);
        view.first = 6;
        assert_eq!(shown(&view), [0, 1]);
        assert!(page(&view, ">").is_none());
        // A tall screen shows them all at once, with no pages.
        assert_eq!(
            layout(&atlas, (640.0, 480.0), &busy())
                .1
                .iter()
                .filter(|i| i.label == "<" || i.label == ">")
                .count(),
            0
        );
    }

    #[test]
    fn taps_and_drags_ask_for_what_they_show() {
        let atlas = atlas();
        let size = (427.0, 240.0);
        let view = busy();
        let (_, items) = layout(&atlas, size, &view);
        let at = |label: &str| {
            let item = items
                .iter()
                .find(|i| i.label.starts_with(label))
                .unwrap_or_else(|| panic!("no {label}"));
            (item.x, item.y, item.w)
        };
        let tap = |x: f32, y: f32| press(&atlas, size, (x, y), &view);
        let (x, y, _) = at("Voice Chat: On");
        assert_eq!(tap(x + 5.0, y + 5.0), Some((Action::Enable(false), None)));
        let (x, y, _) = at("Microphone: On");
        assert_eq!(tap(x + 5.0, y + 5.0), Some((Action::Mic(false), None)));
        let (x, y, _) = at("Mic: iPhone Microphone");
        assert_eq!(tap(x + 5.0, y + 5.0), Some((Action::NextMic, None)));
        // A slider: a tap at its middle is 100 %, its ends 0 and 200 %, and it is held from
        // there for a drag.
        let (x, y, w) = at("Player0");
        let Some((Action::Volume(0, volume), Some(held))) = tap(x + w / 2.0, y + 10.0) else {
            panic!("the slider didn't take the tap");
        };
        assert_eq!(volume, 1.0);
        assert_eq!(
            held,
            Held {
                person: Some(0),
                x,
                w
            }
        );
        assert_eq!(held.action(x - 50.0), Action::Volume(0, 0.0));
        assert_eq!(held.action(x + w + 50.0), Action::Volume(0, MAX_VOLUME));
        // In steps of 5 %.
        for cx in 0..w as i32 {
            let Action::Volume(_, v) = held.action(x + cx as f32) else {
                panic!("not a volume");
            };
            assert!(((v * 20.0).round() - v * 20.0).abs() < 1e-4, "{v}");
        }
        let (x, y, w) = at("Voices");
        assert!(matches!(
            tap(x + w - 1.0, y + 10.0),
            Some((Action::Voices(v), Some(Held { person: None, .. }))) if v == MAX_VOLUME
        ));
        let (x, y, _) = at("Mute");
        assert_eq!(tap(x + 5.0, y + 5.0), Some((Action::Mute(0, true), None)));
        let (x, y, _) = at("Noise Filter: Off");
        assert_eq!(
            tap(x + 5.0, y + 5.0),
            Some((Action::NoiseFilter(true), None))
        );
        // Nowhere: nothing.
        assert_eq!(tap(1.0, 1.0), None);
        // With voice chat off only turning it on (and Back, and muting ahead) can be asked.
        let off = View {
            enabled: false,
            mic_live: false,
            ..busy()
        };
        let (_, items) = layout(&atlas, size, &off);
        let active: Vec<&str> = items
            .iter()
            .filter(|i| i.active)
            .map(|i| i.label.as_str())
            .filter(|l| !l.starts_with("Mute") && !l.starts_with('<') && !l.starts_with('>'))
            .collect();
        assert_eq!(
            active,
            [
                "Voice Chat: Off",
                "Mic: iPhone Microphone",
                "Noise Filter: Off",
                "gui.back"
            ]
        );
        // The host's game has none: nothing but Back.
        let none = View::default();
        let (lines, items) = layout(&atlas, size, &none);
        assert!(
            items
                .iter()
                .filter(|i| i.active)
                .all(|i| i.label == "gui.back")
        );
        assert!(
            lines
                .iter()
                .any(|(l, ..)| l.contains("doesn't have voice chat"))
        );
    }

    #[test]
    fn a_person_is_shown_as_they_are() {
        let atlas = atlas();
        let mut view = busy();
        view.people = vec![
            Person {
                volume: 1.5,
                ..person(1, "Alex")
            },
            Person {
                mic_live: false,
                ..person(2, "Steve")
            },
            Person {
                enabled: false,
                mic_live: false,
                ..person(3, "Sam")
            },
            Person {
                muted: true,
                ..person(4, "Kai")
            },
        ];
        let (_, items) = layout(&atlas, (640.0, 360.0), &view);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"Alex: 150%"));
        assert!(labels.contains(&"Steve (mic off): 100%"));
        assert!(labels.contains(&"Sam (off): 100%"));
        assert!(labels.contains(&"Unmute"));
        // A muted person's slider waits for them to be unmuted, and shows nobody talking.
        let kai = items.iter().find(|i| i.label.starts_with("Kai")).unwrap();
        assert!(!kai.active && !kai.talking);
        let steve = items.iter().find(|i| i.label.starts_with("Steve")).unwrap();
        assert!(steve.talking);
        assert_eq!(percent(0.0), "0%");
        assert_eq!(percent(2.0), "200%");
        assert_eq!(volume_at(0.0, 108.0, 54.0), 1.0);
    }

    #[test]
    fn groups_actions_and_forms_work() {
        let atlas = atlas();
        let size = (427.0, 240.0);
        // People tab shows Groups switch button when groups_enabled.
        let people_view = View {
            groups_enabled: true,
            ..busy()
        };
        let (_, items) = layout(&atlas, size, &people_view);
        let groups_btn = items.iter().find(|i| i.label == "Groups").unwrap();
        assert_eq!(
            press(
                &atlas,
                size,
                (groups_btn.x + 5.0, groups_btn.y + 5.0),
                &people_view
            ),
            Some((Action::Tab(Tab::Groups), None))
        );

        // Groups tab: Join and Leave buttons emit expected actions.
        let groups_view = View {
            groups_enabled: true,
            tab: Tab::Groups,
            joined_group: Some(1),
            groups: vec![
                group(1, "Alpha", false, GroupType::Normal, true),
                group(2, "Beta", true, GroupType::Isolated, false),
            ],
            ..busy()
        };
        let (_, items) = layout(&atlas, size, &groups_view);
        let join_btn = items.iter().find(|i| i.label == "Join").unwrap();
        assert_eq!(
            press(
                &atlas,
                size,
                (join_btn.x + 5.0, join_btn.y + 5.0),
                &groups_view
            ),
            Some((Action::JoinGroup(2), None))
        );
        let leave_btn = items.iter().find(|i| i.label == "Leave").unwrap();
        assert_eq!(
            press(
                &atlas,
                size,
                (leave_btn.x + 5.0, leave_btn.y + 5.0),
                &groups_view
            ),
            Some((Action::LeaveGroup, None))
        );

        // Create Group form: Name/Password fields focus, Type cycles, Create submits.
        let create_view = View {
            groups_enabled: true,
            tab: Tab::Groups,
            create_mode: true,
            name_input: "Raiders".into(),
            group_type: GroupType::Open,
            ..busy()
        };
        let (_, items) = layout(&atlas, size, &create_view);
        let fields: Vec<&Item> = items
            .iter()
            .filter(|i| matches!(i.kind, Kind::Field { .. }))
            .collect();
        assert_eq!(fields.len(), 2);
        assert_eq!(
            press(
                &atlas,
                size,
                (fields[0].x + 5.0, fields[0].y + 5.0),
                &create_view
            ),
            Some((Action::FocusField(GroupField::Name), None))
        );
        assert_eq!(
            press(
                &atlas,
                size,
                (fields[1].x + 5.0, fields[1].y + 5.0),
                &create_view
            ),
            Some((Action::FocusField(GroupField::Password), None))
        );
        let create_btn = items.iter().find(|i| i.label == "Create").unwrap();
        assert_eq!(
            press(
                &atlas,
                size,
                (create_btn.x + 5.0, create_btn.y + 5.0),
                &create_view
            ),
            Some((Action::SubmitCreateGroup, None))
        );
    }
}
