//! Classic Minecraft Bedrock / Pocket Edition touch controls:
//! translucent beveled tiles, draggable 3x3 D-pad with diagonal corners and double-tap sprint,
//! hotbar-attached inventory button, top-center Pause / Chat / `...` drawer, Touch-to-Interact
//! touch circle with smooth 60 FPS growing break-progress disc, compact on-screen mod/voice tiles
//! (Zoom, Waypoints, Voice Chat), and an in-game drag/resize layout editor persisted to
//! `<home>/classic-pe-layout.json`.

use crate::gamepad::{Button, PadState};
use crate::hud::{HudAtlas, HudBatch, rgba};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Persisted custom layout file in the game's home directory.
const LAYOUT_FILE: &str = "classic-pe-layout.json";
/// Maximum interval (seconds) between two `Forward` presses to trigger sprint.
const DOUBLE_TAP_SPRINT_WINDOW: f64 = 0.35;
/// How far a finger moves (GUI pixels) before a world touch is a look drag instead of a tap.
const SLOP: f32 = 8.0;
/// How long a finger stays still (seconds) on the world before mining / hold-use engages.
const HOLD: f64 = 0.35;
/// Pre-mine dwell delay before the touch circle starts fading in under a still finger.
const DWELL_SHOW: f64 = 0.10;
/// Maximum press duration (seconds) for a Zoom tap to toggle zoom instead of hold-to-zoom.
const ZOOM_TAP_WINDOW: f64 = 0.25;
/// Gap kept from the screen's edges in GUI pixels.
const MARGIN: f32 = 12.0;
/// How far above the hotbar its slots still accept a tap (thumbs land slightly high).
const HOTBAR_REACH: f32 = 8.0;

/// `[x, y, width, height]` in GUI pixels.
pub type Rect = [f32; 4];

/// Touch phase independent of winit so this module stays pure & unit-testable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TouchPhaseKind {
    Started,
    Moved,
    Ended,
    Cancelled,
}

/// A button or interactive region in the classic Bedrock / Pocket Edition touch layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClassicPeControl {
    Forward,
    Back,
    Left,
    Right,
    /// Center of the 3x3 D-pad: crouch toggle.
    Sneak,
    Jump,
    /// Contextual descend button below `Jump` while flying, swimming, mounted, or editing.
    FlyDown,
    Pause,
    Chat,
    More,
    Inventory,
    Hotbar(u8),
    /// Optional dedicated Attack button (toggled via `ATK` in the `...` drawer).
    LeftClick,
    /// Optional dedicated Use button (toggled via `ATK` in the `...` drawer).
    RightClick,
    /// Built-in Zoom mod button (left edge, gold magnifying glass when active).
    Zoom,
    /// Built-in Waymap waypoints list button.
    Waypoints,
    /// Built-in Waymap quick add-waypoint button.
    AddWaypoint,
    /// Built-in Voice Chat microphone mute button (red when muted).
    Voice,
    /// Drawer: F3 debug overlay toggle.
    Debug,
    /// Drawer: F5 camera perspective cycle.
    Perspective,
    /// Drawer: TAB player list toggle.
    PlayerList,
    /// Drawer: Q drop held item.
    Drop,
    /// Drawer: toggle Split Controls (center crosshair) vs. Touch-to-Interact (touch circle).
    ToggleSplitControls,
    /// Drawer: toggle optional dedicated Attack / Use buttons.
    ToggleActionButtons,
    /// Drawer: hide / restore touch buttons.
    Hide,
    /// Drawer: open in-game drag/resize layout editor.
    EditLayout,
}

impl ClassicPeControl {
    /// Whether this control is one of the four D-pad cardinal directions.
    pub fn moves(self) -> bool {
        matches!(self, Self::Forward | Self::Back | Self::Left | Self::Right)
    }

    /// Movable/resizable group that owns this control in the layout editor.
    pub fn group(self) -> Option<LayoutGroup> {
        match self {
            Self::Forward | Self::Back | Self::Left | Self::Right | Self::Sneak => {
                Some(LayoutGroup::Dpad)
            }
            Self::Jump | Self::FlyDown => Some(LayoutGroup::Jump),
            Self::Pause | Self::Chat | Self::More => Some(LayoutGroup::TopBar),
            Self::Inventory => Some(LayoutGroup::Inventory),
            Self::LeftClick => Some(LayoutGroup::Attack),
            Self::RightClick => Some(LayoutGroup::Use),
            Self::Zoom => Some(LayoutGroup::Zoom),
            Self::Waypoints | Self::AddWaypoint => Some(LayoutGroup::Waypoints),
            Self::Voice => Some(LayoutGroup::Voice),
            _ => None,
        }
    }
}

/// Movable/resizable control clusters in the in-game drag/resize layout editor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayoutGroup {
    Dpad,
    Jump,
    TopBar,
    Inventory,
    Attack,
    Use,
    Zoom,
    Waypoints,
    Voice,
}

impl LayoutGroup {
    pub const ALL: [Self; 9] = [
        Self::Dpad,
        Self::Jump,
        Self::TopBar,
        Self::Inventory,
        Self::Attack,
        Self::Use,
        Self::Zoom,
        Self::Waypoints,
        Self::Voice,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Dpad => "D-Pad",
            Self::Jump => "Jump",
            Self::TopBar => "Top Bar",
            Self::Inventory => "Inventory",
            Self::Attack => "Attack",
            Self::Use => "Use",
            Self::Zoom => "Zoom",
            Self::Waypoints => "Waypoints",
            Self::Voice => "Voice",
        }
    }

    fn is_top_region(self) -> bool {
        matches!(self, Self::TopBar | Self::Waypoints | Self::Voice)
    }
}

fn default_scale() -> f32 {
    1.0
}

fn default_opacity() -> f32 {
    0.72
}

/// Normalized placement and scale multiplier for one movable control group.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct GroupPlacement {
    /// Normalized center X in `[0.0, 1.0]`.
    pub x: f32,
    /// Normalized center Y in `[0.0, 1.0]`.
    pub y: f32,
    /// Scale multiplier in `[0.6, 1.6]`.
    #[serde(default = "default_scale")]
    pub scale: f32,
}

impl GroupPlacement {
    pub fn sanitized(self) -> Option<Self> {
        if !self.x.is_finite() || !self.y.is_finite() || !self.scale.is_finite() {
            return None;
        }
        Some(Self {
            x: self.x.clamp(0.0, 1.0),
            y: self.y.clamp(0.0, 1.0),
            scale: self.scale.clamp(0.6, 1.6),
        })
    }
}

/// Persisted classic Bedrock / Pocket Edition layout preferences (`<home>/classic-pe-layout.json`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClassicPeConfig {
    #[serde(default = "default_opacity")]
    pub opacity: f32,
    #[serde(default)]
    pub show_action_buttons: bool,
    /// `false` (default): classic PE Touch-to-Interact (touch circle, crosshair hidden).
    /// `true`: Split Controls (center crosshair).
    #[serde(default)]
    pub split_controls: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dpad: Option<GroupPlacement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jump: Option<GroupPlacement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_bar: Option<GroupPlacement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inventory: Option<GroupPlacement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attack: Option<GroupPlacement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_btn: Option<GroupPlacement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zoom: Option<GroupPlacement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waypoints: Option<GroupPlacement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<GroupPlacement>,
}

impl Default for ClassicPeConfig {
    fn default() -> Self {
        Self {
            opacity: default_opacity(),
            show_action_buttons: false,
            split_controls: false,
            dpad: None,
            jump: None,
            top_bar: None,
            inventory: None,
            attack: None,
            use_btn: None,
            zoom: None,
            waypoints: None,
            voice: None,
        }
    }
}

impl ClassicPeConfig {
    pub fn path(home: &Path) -> PathBuf {
        home.join(LAYOUT_FILE)
    }

    pub fn load(home: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(Self::path(home)) else {
            return Self::default();
        };
        let Ok(mut cfg) = serde_json::from_str::<Self>(&text) else {
            return Self::default();
        };
        cfg.sanitize();
        cfg
    }

    pub fn save(&self, home: &Path) -> std::io::Result<()> {
        let mut clean = self.clone();
        clean.sanitize();
        std::fs::create_dir_all(home)?;
        let json = serde_json::to_string_pretty(&clean)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(Self::path(home), json)
    }

    pub fn sanitize(&mut self) {
        self.opacity = if self.opacity.is_finite() {
            self.opacity.clamp(0.25, 1.0)
        } else {
            default_opacity()
        };
        self.dpad = self.dpad.and_then(GroupPlacement::sanitized);
        self.jump = self.jump.and_then(GroupPlacement::sanitized);
        self.top_bar = self.top_bar.and_then(GroupPlacement::sanitized);
        self.inventory = self.inventory.and_then(GroupPlacement::sanitized);
        self.attack = self.attack.and_then(GroupPlacement::sanitized);
        self.use_btn = self.use_btn.and_then(GroupPlacement::sanitized);
        self.zoom = self.zoom.and_then(GroupPlacement::sanitized);
        self.waypoints = self.waypoints.and_then(GroupPlacement::sanitized);
        self.voice = self.voice.and_then(GroupPlacement::sanitized);
    }

    pub fn reset(&mut self) {
        let keep_actions = self.show_action_buttons;
        let keep_split = self.split_controls;
        *self = Self::default();
        self.show_action_buttons = keep_actions;
        self.split_controls = keep_split;
    }

    pub fn group(&self, g: LayoutGroup) -> Option<GroupPlacement> {
        match g {
            LayoutGroup::Dpad => self.dpad,
            LayoutGroup::Jump => self.jump,
            LayoutGroup::TopBar => self.top_bar,
            LayoutGroup::Inventory => self.inventory,
            LayoutGroup::Attack => self.attack,
            LayoutGroup::Use => self.use_btn,
            LayoutGroup::Zoom => self.zoom,
            LayoutGroup::Waypoints => self.waypoints,
            LayoutGroup::Voice => self.voice,
        }
    }

    pub fn set_group(&mut self, g: LayoutGroup, placement: GroupPlacement) {
        let p = placement.sanitized();
        match g {
            LayoutGroup::Dpad => self.dpad = p,
            LayoutGroup::Jump => self.jump = p,
            LayoutGroup::TopBar => self.top_bar = p,
            LayoutGroup::Inventory => self.inventory = p,
            LayoutGroup::Attack => self.attack = p,
            LayoutGroup::Use => self.use_btn = p,
            LayoutGroup::Zoom => self.zoom = p,
            LayoutGroup::Waypoints => self.waypoints = p,
            LayoutGroup::Voice => self.voice = p,
        }
    }

    pub fn scale_of(&self, g: LayoutGroup) -> f32 {
        self.group(g).map_or(1.0, |p| p.scale)
    }
}

/// Dynamic visibility flags for building the classic Bedrock / Pocket Edition layout.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LayoutOptions {
    pub more_open: bool,
    pub hidden: bool,
    pub vertical_movement: bool,
    pub editing: bool,
    pub zoom_available: bool,
    pub waypoints_available: bool,
    pub voice_available: bool,
}

/// Base button size for a `h` GUI-pixel-tall viewport.
pub fn button_size(h: f32) -> f32 {
    (h * 0.14).clamp(24.0, 44.0)
}

fn clamp_box(cx: f32, cy: f32, bw: f32, bh: f32, w: f32, h: f32) -> [f32; 2] {
    let x = (cx - bw / 2.0).clamp(0.0, (w - bw).max(0.0));
    let y = (cy - bh / 2.0).clamp(0.0, (h - bh).max(0.0));
    [x, y]
}

fn contains(r: Rect, (x, y): (f32, f32)) -> bool {
    x >= r[0] && x < r[0] + r[2] && y >= r[1] && y < r[1] + r[3]
}

/// Bounding box of a movable group within a built layout.
pub fn group_bounds(layout: &[(ClassicPeControl, Rect)], group: LayoutGroup) -> Option<Rect> {
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    let mut found = false;
    for (c, r) in layout {
        if c.group() == Some(group) {
            found = true;
            min_x = min_x.min(r[0]);
            min_y = min_y.min(r[1]);
            max_x = max_x.max(r[0] + r[2]);
            max_y = max_y.max(r[1] + r[3]);
        }
    }
    found.then_some([min_x, min_y, max_x - min_x, max_y - min_y])
}

/// Build the classic Bedrock / Pocket Edition touch layout for a `(w, h)` GUI-pixel viewport.
pub fn bedrock_layout(
    w: f32,
    h: f32,
    config: &ClassicPeConfig,
    opts: LayoutOptions,
) -> Vec<(ClassicPeControl, Rect)> {
    let mut v = Vec::with_capacity(36);

    // 1. Hotbar slots: vanilla 182x22 centered at bottom.
    let (hx, hy) = ((w / 2.0 - 91.0).floor().max(0.0), (h - 22.0).max(0.0));
    let hotbar_reach = HOTBAR_REACH.min(hy);
    for slot in 0..9u8 {
        let sx = hx + 1.0 + 20.0 * f32::from(slot);
        let sw = 20.0_f32.min((w - sx).max(0.0));
        if sw > 0.0 {
            v.push((
                ClassicPeControl::Hotbar(slot),
                [sx, hy - hotbar_reach, sw, (22.0 + hotbar_reach).min(h)],
            ));
        }
    }

    let top_scale = config.scale_of(LayoutGroup::TopBar);
    let ts = (h * 0.085 * top_scale)
        .clamp(18.0, 28.0)
        .min((w / 4.0).max(8.0))
        .min((h / 4.0).max(8.0));

    // When hidden (and not editing), leave only a subtle top-center restore tile + hotbar.
    if opts.hidden && !opts.editing {
        let rw = (ts * 1.4).min(w);
        let rh = (ts * 0.72).clamp(12.0, h);
        v.push((
            ClassicPeControl::Hide,
            [((w - rw) / 2.0).floor(), 2.0, rw, rh],
        ));
        return v;
    }

    let margin = MARGIN.min(w.min(h) / 6.0);
    let base_d = button_size(h);

    // 2. Inventory button attached to the right edge of the hotbar (classic PE "..." slot).
    let inv_scale = config.scale_of(LayoutGroup::Inventory);
    let inv_s = (22.0 * inv_scale).clamp(16.0, 40.0).min(w).min(h);
    let default_inv_cx = (hx + 182.0 + 2.0 + inv_s / 2.0).min(w - inv_s / 2.0);
    let default_inv_cy = h - inv_s / 2.0;
    let [ix, iy] = match config.inventory {
        Some(p) => clamp_box(p.x * w, p.y * h, inv_s, inv_s, w, h),
        None => clamp_box(default_inv_cx, default_inv_cy, inv_s, inv_s, w, h),
    };
    v.push((ClassicPeControl::Inventory, [ix, iy, inv_s, inv_s]));

    // 3. D-Pad (3x3 cross: Forward, Back, Left, Right + center Sneak).
    let dpad_scale = config.scale_of(LayoutGroup::Dpad);
    let max_d_for_hotbar = if w >= 260.0 {
        ((hx - margin - 4.0).max(36.0) / 3.0).max(16.0)
    } else {
        (w - margin) / 3.0
    };
    let d = (base_d * dpad_scale)
        .min(max_d_for_hotbar)
        .min((w - margin) / 3.0)
        .min((h - margin - ts - 6.0).max(18.0) / 3.0)
        .max(6.0);
    let dpad_span = 3.0 * d;
    let default_dpad_cx = margin + 1.5 * d;
    let default_dpad_cy = (h - margin - 1.5 * d - 6.0).max(1.5 * d);
    let [dx, dy] = match config.dpad {
        Some(p) => clamp_box(p.x * w, p.y * h, dpad_span, dpad_span, w, h),
        None => clamp_box(default_dpad_cx, default_dpad_cy, dpad_span, dpad_span, w, h),
    };
    v.extend([
        (ClassicPeControl::Forward, [dx + d, dy, d, d]),
        (ClassicPeControl::Back, [dx + d, dy + 2.0 * d, d, d]),
        (ClassicPeControl::Left, [dx, dy + d, d, d]),
        (ClassicPeControl::Right, [dx + 2.0 * d, dy + d, d, d]),
        (ClassicPeControl::Sneak, [dx + d, dy + d, d, d]),
    ]);

    // 4. Jump (+ contextual FlyDown when flying / swimming / mounted or in editor).
    let jump_scale = config.scale_of(LayoutGroup::Jump);
    let js = (base_d * 1.15 * jump_scale)
        .clamp(22.0, 56.0)
        .min(w * 0.28)
        .min(h * 0.32);
    let show_fly_down = opts.vertical_movement || opts.editing;
    let jump_gap = 4.0;
    let jump_h = if show_fly_down {
        js * 2.0 + jump_gap
    } else {
        js
    };
    let default_jump_cx = (w - margin - js * 0.85)
        .max(ix + inv_s + js / 2.0 + 4.0)
        .min(w - js / 2.0);
    let default_jump_cy = (dy + 1.5 * d).min(h - margin - jump_h / 2.0 - 4.0);
    let [jx, jy] = match config.jump {
        Some(p) => clamp_box(p.x * w, p.y * h, js, jump_h, w, h),
        None => clamp_box(default_jump_cx, default_jump_cy, js, jump_h, w, h),
    };
    v.push((ClassicPeControl::Jump, [jx, jy, js, js]));
    if show_fly_down {
        v.push((ClassicPeControl::FlyDown, [jx, jy + js + jump_gap, js, js]));
    }

    // 5. Optional dedicated Attack (LeftClick) & Use (RightClick) buttons.
    if config.show_action_buttons || opts.editing {
        let atk_scale = config.scale_of(LayoutGroup::Attack);
        let atk_s = (base_d * 0.92 * atk_scale)
            .clamp(20.0, 48.0)
            .min(w * 0.24)
            .min(h * 0.26);
        let default_atk_cx = (jx - atk_s * 0.5 - 8.0).max(atk_s / 2.0);
        let default_atk_cy = (jy + js * 0.35).min(hy - hotbar_reach - atk_s / 2.0 - 4.0);
        let [ax, ay] = match config.attack {
            Some(p) => clamp_box(p.x * w, p.y * h, atk_s, atk_s, w, h),
            None => clamp_box(default_atk_cx, default_atk_cy, atk_s, atk_s, w, h),
        };
        v.push((ClassicPeControl::LeftClick, [ax, ay, atk_s, atk_s]));

        let use_scale = config.scale_of(LayoutGroup::Use);
        let use_s = (base_d * 0.92 * use_scale)
            .clamp(20.0, 48.0)
            .min(w * 0.24)
            .min(h * 0.26);
        let default_use_cx = jx + js * 0.5;
        let default_use_cy = (jy - use_s * 0.5 - 8.0).max(ts + 8.0 + use_s / 2.0);
        let [ux, uy] = match config.use_btn {
            Some(p) => clamp_box(p.x * w, p.y * h, use_s, use_s, w, h),
            None => clamp_box(default_use_cx, default_use_cy, use_s, use_s, w, h),
        };
        v.push((ClassicPeControl::RightClick, [ux, uy, use_s, use_s]));
    }

    // 6. Top-middle bar: Pause, Chat, More ("...").
    let top_gap = 3.0;
    let top_w = (3.0 * ts + 2.0 * top_gap).min(w);
    let [tx, ty] = match config.top_bar {
        Some(p) => clamp_box(p.x * w, p.y * h, top_w, ts, w, h),
        None => clamp_box(w / 2.0, 4.0 + ts / 2.0, top_w, ts, w, h),
    };
    v.extend([
        (ClassicPeControl::Pause, [tx, ty, ts, ts]),
        (ClassicPeControl::Chat, [tx + ts + top_gap, ty, ts, ts]),
        (
            ClassicPeControl::More,
            [tx + 2.0 * (ts + top_gap), ty, ts, ts],
        ),
    ]);

    // 7. Compact on-screen Horizon PE mod & voice tiles (classic PE style, movable in EDIT).
    if opts.zoom_available || opts.editing {
        let zoom_scale = config.scale_of(LayoutGroup::Zoom);
        let zs = (ts * 1.1 * zoom_scale)
            .clamp(18.0, 34.0)
            .min(w * 0.2)
            .min(h * 0.22);
        let default_zx = margin + zs / 2.0;
        let default_zy = (dy - zs / 2.0 - 8.0).max(ty + ts + 8.0 + zs / 2.0);
        let [zx, zy] = match config.zoom {
            Some(p) => clamp_box(p.x * w, p.y * h, zs, zs, w, h),
            None => clamp_box(default_zx, default_zy, zs, zs, w, h),
        };
        v.push((ClassicPeControl::Zoom, [zx, zy, zs, zs]));
    }

    if opts.waypoints_available || opts.editing {
        let wp_scale = config.scale_of(LayoutGroup::Waypoints);
        let ws = (ts * wp_scale)
            .clamp(16.0, 32.0)
            .min((w / 5.0).max(8.0))
            .min((h / 5.0).max(8.0));
        let wp_w = 2.0 * ws + top_gap;
        let default_wx = (tx - 6.0 - wp_w / 2.0).max(margin + wp_w / 2.0);
        let default_wy = ty + ws / 2.0;
        let [wx, wy] = match config.waypoints {
            Some(p) => clamp_box(p.x * w, p.y * h, wp_w, ws, w, h),
            None => clamp_box(default_wx, default_wy, wp_w, ws, w, h),
        };
        v.push((ClassicPeControl::Waypoints, [wx, wy, ws, ws]));
        v.push((
            ClassicPeControl::AddWaypoint,
            [wx + ws + top_gap, wy, ws, ws],
        ));
    }

    if opts.voice_available || opts.editing {
        let vc_scale = config.scale_of(LayoutGroup::Voice);
        let vs = (ts * vc_scale)
            .clamp(16.0, 32.0)
            .min((w / 5.0).max(8.0))
            .min((h / 5.0).max(8.0));
        let default_vx = (tx + top_w + 6.0 + vs / 2.0).min(w - margin - vs / 2.0);
        let default_vy = ty + vs / 2.0;
        let [vx, vy] = match config.voice {
            Some(p) => clamp_box(p.x * w, p.y * h, vs, vs, w, h),
            None => clamp_box(default_vx, default_vy, vs, vs, w, h),
        };
        v.push((ClassicPeControl::Voice, [vx, vy, vs, vs]));
    }

    // 8. Expandable "..." extras drawer directly below the top bar.
    if opts.more_open && !opts.editing {
        let drawer = [
            ClassicPeControl::Debug,
            ClassicPeControl::Perspective,
            ClassicPeControl::PlayerList,
            ClassicPeControl::Drop,
            ClassicPeControl::ToggleSplitControls,
            ClassicPeControl::ToggleActionButtons,
            ClassicPeControl::Hide,
            ClassicPeControl::EditLayout,
        ];
        let dgap = 2.5;
        let dh = (ts * 0.92).clamp(16.0, 24.0);
        let max_row_w = (w - 8.0).max(dh);
        let dw = (ts * 1.45).clamp(24.0, 38.0).min(max_row_w);
        let per_row = (((max_row_w + dgap) / (dw + dgap)).floor() as usize).clamp(1, drawer.len());
        let rows = drawer.len().div_ceil(per_row);
        let drawer_h = rows as f32 * dh + (rows.saturating_sub(1)) as f32 * dgap;
        let below_y = ty + ts + 4.0;
        let start_y = if below_y + drawer_h <= h - 24.0 {
            below_y
        } else {
            (ty - 4.0 - drawer_h).max(2.0)
        };
        let bar_cx = tx + top_w / 2.0;
        for (idx, control) in drawer.into_iter().enumerate() {
            let row = idx / per_row;
            let col = idx % per_row;
            let row_count = if (row + 1) * per_row <= drawer.len() {
                per_row
            } else {
                drawer.len() - row * per_row
            };
            let row_w = row_count as f32 * dw + (row_count.saturating_sub(1)) as f32 * dgap;
            let row_x = (bar_cx - row_w / 2.0).clamp(4.0, (w - 4.0 - row_w).max(0.0));
            let bx = row_x + col as f32 * (dw + dgap);
            let by = start_y + row as f32 * (dh + dgap);
            v.push((control, [bx, by, dw, dh]));
        }
    }

    v
}

/// Hit-test a point against the layout (buttons paint over hotbar; later buttons take priority).
pub fn hit(layout: &[(ClassicPeControl, Rect)], at: (f32, f32)) -> Option<ClassicPeControl> {
    layout
        .iter()
        .rev()
        .find(|(c, r)| !matches!(c, ClassicPeControl::Hotbar(_)) && contains(*r, at))
        .or_else(|| layout.iter().find(|(_, r)| contains(*r, at)))
        .map(|(c, _)| *c)
}

/// Direction bitmask for the 3x3 D-pad.
pub const DPAD_FORWARD: u8 = 1 << 0;
pub const DPAD_LEFT: u8 = 1 << 1;
pub const DPAD_BACK: u8 = 1 << 2;
pub const DPAD_RIGHT: u8 = 1 << 3;

const DPAD_DIRS: [(u8, ClassicPeControl); 4] = [
    (DPAD_FORWARD, ClassicPeControl::Forward),
    (DPAD_LEFT, ClassicPeControl::Left),
    (DPAD_BACK, ClassicPeControl::Back),
    (DPAD_RIGHT, ClassicPeControl::Right),
];

fn dir_mask(control: ClassicPeControl) -> u8 {
    match control {
        ClassicPeControl::Forward => DPAD_FORWARD,
        ClassicPeControl::Left => DPAD_LEFT,
        ClassicPeControl::Back => DPAD_BACK,
        ClassicPeControl::Right => DPAD_RIGHT,
        _ => 0,
    }
}

/// Sample a point inside the 3x3 D-pad bounding square, including diagonal corners.
/// Returns `None` outside the 3x3 square or in the center Sneak cell.
fn dpad_sample(bounds: Rect, at: (f32, f32)) -> Option<u8> {
    if !contains(bounds, at) || bounds[2] <= 0.0 || bounds[3] <= 0.0 {
        return None;
    }
    let rx = (at.0 - bounds[0]) / bounds[2];
    let ry = (at.1 - bounds[1]) / bounds[3];
    let col = if rx < 1.0 / 3.0 {
        0
    } else if rx >= 2.0 / 3.0 {
        2
    } else {
        1
    };
    let row = if ry < 1.0 / 3.0 {
        0
    } else if ry >= 2.0 / 3.0 {
        2
    } else {
        1
    };
    let mut mask = 0u8;
    if row == 0 {
        mask |= DPAD_FORWARD;
    } else if row == 2 {
        mask |= DPAD_BACK;
    }
    if col == 0 {
        mask |= DPAD_LEFT;
    } else if col == 2 {
        mask |= DPAD_RIGHT;
    }
    (mask != 0).then_some(mask)
}

/// Per-event output from [`ClassicPeTouches`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TouchOut {
    Press(ClassicPeControl),
    Release(ClassicPeControl),
    Look(f32, f32),
    Tap((f32, f32)),
    MineStart((f32, f32)),
    MineEnd,
}

#[derive(Clone, Debug)]
enum FingerKind {
    Button(ClassicPeControl),
    Dpad { mask: u8, sprinting: bool },
    World { dragged: bool, mining: bool },
}

#[derive(Clone, Debug)]
struct Finger {
    id: u64,
    kind: FingerKind,
    start: (f32, f32),
    last: (f32, f32),
    since: f64,
}

/// Multi-touch gesture recognizer for classic Bedrock / Pocket Edition gameplay.
#[derive(Clone, Debug, Default)]
pub struct ClassicPeTouches {
    last_forward_tap: Option<f64>,
    last_world_pos: Option<(f32, f32)>,
    fingers: Vec<Finger>,
}

impl ClassicPeTouches {
    pub fn has_contact(&self) -> bool {
        !self.fingers.is_empty()
    }

    pub fn last_world_pos(&self) -> Option<(f32, f32)> {
        self.last_world_pos
    }

    pub fn active_mine_pos(&self) -> Option<(f32, f32)> {
        self.fingers.iter().find_map(|f| match f.kind {
            FingerKind::World { mining: true, .. } => Some(f.last),
            _ => None,
        })
    }

    /// Pre-mine dwell feedback: once a still world touch has been held past `0.10s`,
    /// returns `(pos, fade_in_fraction)` up until `HOLD` engages mining.
    pub fn pending_hold_pos(&self, now: f64) -> Option<((f32, f32), f32)> {
        self.fingers.iter().find_map(|f| match f.kind {
            FingerKind::World {
                dragged: false,
                mining: false,
            } => {
                let elapsed = now - f.since;
                if elapsed >= DWELL_SHOW {
                    let frac = ((elapsed - DWELL_SHOW) / (HOLD - DWELL_SHOW)).clamp(0.0, 1.0);
                    Some((f.last, frac as f32))
                } else {
                    None
                }
            }
            _ => None,
        })
    }

    pub fn dpad_mask(&self) -> u8 {
        self.fingers.iter().fold(0u8, |acc, f| match f.kind {
            FingerKind::Dpad { mask, .. } => acc | mask,
            _ => acc,
        })
    }

    pub fn sprinting(&self) -> bool {
        self.fingers.iter().any(|f| {
            matches!(
                f.kind,
                FingerKind::Dpad {
                    sprinting: true,
                    ..
                }
            )
        })
    }

    pub fn held(&self, control: ClassicPeControl) -> bool {
        self.fingers.iter().any(|f| match f.kind {
            FingerKind::Button(c) => c == control,
            FingerKind::Dpad { mask, .. } => DPAD_DIRS
                .iter()
                .any(|(bit, c)| *c == control && mask & bit != 0),
            FingerKind::World { .. } => false,
        })
    }

    pub fn start(
        &mut self,
        id: u64,
        at: (f32, f32),
        now: f64,
        layout: &[(ClassicPeControl, Rect)],
    ) -> Vec<TouchOut> {
        let mut out = self.cancel(id);
        let hit_control = hit(layout, at);
        let kind = match hit_control {
            Some(control) if control.moves() => {
                let mask = dir_mask(control);
                emit_mask_diff(0, mask, &mut out);
                let sprinting = self.check_double_tap_forward(mask, now);
                FingerKind::Dpad { mask, sprinting }
            }
            Some(control) => {
                out.push(TouchOut::Press(control));
                FingerKind::Button(control)
            }
            None => {
                if let Some(bounds) = group_bounds(layout, LayoutGroup::Dpad)
                    && let Some(mask) = dpad_sample(bounds, at)
                {
                    emit_mask_diff(0, mask, &mut out);
                    let sprinting = self.check_double_tap_forward(mask, now);
                    FingerKind::Dpad { mask, sprinting }
                } else {
                    FingerKind::World {
                        dragged: false,
                        mining: false,
                    }
                }
            }
        };
        if matches!(kind, FingerKind::World { .. }) {
            self.last_world_pos = Some(at);
        }
        self.fingers.push(Finger {
            id,
            kind,
            start: at,
            last: at,
            since: now,
        });
        out
    }

    fn check_double_tap_forward(&mut self, mask: u8, now: f64) -> bool {
        if mask & DPAD_FORWARD == 0 {
            self.last_forward_tap = None;
            return false;
        }
        if let Some(prev) = self.last_forward_tap
            && now >= prev
            && now - prev <= DOUBLE_TAP_SPRINT_WINDOW
        {
            self.last_forward_tap = None;
            true
        } else {
            self.last_forward_tap = Some(now);
            false
        }
    }

    pub fn moved(
        &mut self,
        id: u64,
        at: (f32, f32),
        layout: &[(ClassicPeControl, Rect)],
        attack_button_look: bool,
    ) -> Vec<TouchOut> {
        let Some(f) = self.fingers.iter_mut().find(|f| f.id == id) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        match &mut f.kind {
            FingerKind::Dpad { mask, sprinting } => {
                let next_mask = group_bounds(layout, LayoutGroup::Dpad)
                    .and_then(|bounds| dpad_sample(bounds, at))
                    .or_else(|| hit(layout, at).filter(|c| c.moves()).map(dir_mask));
                if let Some(next) = next_mask
                    && next != *mask
                {
                    emit_mask_diff(*mask, next, &mut out);
                    *mask = next;
                    if *sprinting && (*mask & DPAD_FORWARD == 0) {
                        *sprinting = false;
                    }
                }
            }
            FingerKind::Button(ClassicPeControl::LeftClick | ClassicPeControl::RightClick)
                if attack_button_look =>
            {
                out.push(TouchOut::Look(at.0 - f.last.0, at.1 - f.last.1));
            }
            FingerKind::Button(_) => {}
            FingerKind::World { dragged, mining } => {
                let (dx, dy) = (at.0 - f.start.0, at.1 - f.start.1);
                let threshold = if *mining { 3.0 } else { SLOP };
                if !*dragged && dx.hypot(dy) > threshold {
                    *dragged = true;
                }
                if *dragged {
                    out.push(TouchOut::Look(at.0 - f.last.0, at.1 - f.last.1));
                }
                self.last_world_pos = Some(at);
            }
        }
        f.last = at;
        out
    }

    pub fn end_at(&mut self, id: u64, at: (f32, f32)) -> Vec<TouchOut> {
        if !at.0.is_finite() || !at.1.is_finite() {
            return self.cancel(id);
        }
        let mut out = Vec::new();
        if let Some(finger) = self.fingers.iter().find(|f| f.id == id)
            && matches!(finger.kind, FingerKind::World { .. })
            && finger.last != at
        {
            out = self.moved(id, at, &[], false);
        }
        out.extend(self.end(id));
        out
    }

    pub fn end(&mut self, id: u64) -> Vec<TouchOut> {
        let Some(i) = self.fingers.iter().position(|f| f.id == id) else {
            return Vec::new();
        };
        let finger = self.fingers.remove(i);
        match finger.kind {
            FingerKind::Dpad { mask, .. } => {
                let mut out = Vec::new();
                for (bit, control) in DPAD_DIRS {
                    if mask & bit != 0 && !self.held(control) {
                        out.push(TouchOut::Release(control));
                    }
                }
                out
            }
            FingerKind::Button(control) => {
                if self.held(control) {
                    Vec::new()
                } else {
                    vec![TouchOut::Release(control)]
                }
            }
            FingerKind::World { mining: true, .. } => {
                self.last_world_pos = Some(finger.last);
                vec![TouchOut::MineEnd]
            }
            FingerKind::World { dragged: false, .. } => {
                self.last_world_pos = Some(finger.last);
                vec![TouchOut::Tap(finger.last)]
            }
            FingerKind::World { .. } => Vec::new(),
        }
    }

    pub fn cancel(&mut self, id: u64) -> Vec<TouchOut> {
        let mut out = self.end(id);
        out.retain(|o| !matches!(o, TouchOut::Tap(_)));
        out
    }

    pub fn frame(&mut self, now: f64) -> Vec<TouchOut> {
        let mut out = Vec::new();
        for f in &mut self.fingers {
            if let FingerKind::World {
                dragged: false,
                mining,
            } = &mut f.kind
                && !*mining
                && now - f.since >= HOLD
            {
                *mining = true;
                self.last_world_pos = Some(f.last);
                out.push(TouchOut::MineStart(f.last));
            }
        }
        out
    }

    pub fn release_all(&mut self) -> Vec<TouchOut> {
        let ids: Vec<u64> = self.fingers.iter().map(|f| f.id).collect();
        ids.into_iter().flat_map(|id| self.cancel(id)).collect()
    }
}

fn emit_mask_diff(old: u8, new: u8, out: &mut Vec<TouchOut>) {
    for (bit, control) in DPAD_DIRS {
        if old & bit != 0 && new & bit == 0 {
            out.push(TouchOut::Release(control));
        }
    }
    for (bit, control) in DPAD_DIRS {
        if old & bit == 0 && new & bit != 0 {
            out.push(TouchOut::Press(control));
        }
    }
}

/// Interactive drag/resize state for the in-game classic PE layout editor.
#[derive(Clone, Debug, Default)]
pub struct ClassicPeEditor {
    pub active: bool,
    pub selected: Option<LayoutGroup>,
    dragging: Option<(u64, LayoutGroup, (f32, f32))>,
}

/// Immediate events produced by a single touch event.
#[derive(Clone, Debug, Default)]
pub struct TouchStepEvents {
    /// Camera look delta in GUI pixels `(dx, dy)`.
    pub look_gui: [f32; 2],
    /// Selected hotbar slot (`0..9`) if tapped.
    pub hotbar_slot: Option<usize>,
    /// Short tap on the world (Touch-to-Interact or Split Controls).
    pub tap_world: bool,
    /// Normalized screen point `[0..1, 0..1]` for the tap (if Touch-to-Interact is active).
    pub tap_aim_norm: Option<[f32; 2]>,
    pub pause: bool,
    pub inventory: bool,
    pub chat: bool,
    pub perspective: bool,
    pub debug: bool,
    pub player_list_toggled: Option<bool>,
    pub drop: bool,
    pub voice: bool,
    pub waypoints: bool,
    pub add_waypoint: bool,
    pub clicked: bool,
}

/// Per-frame state produced by [`ClassicPeState::frame_update`].
#[derive(Clone, Debug, Default)]
pub struct FrameUpdate {
    pub pad: PadState,
    pub sneaking: bool,
    /// True while passive crouch-to-shield is holding the shield raised.
    pub sneak_shield: bool,
    pub zoom_active: bool,
    pub aim_norm: Option<[f32; 2]>,
    pub holding_use: bool,
    pub clicked: bool,
}

/// Complete classic Bedrock / Pocket Edition touch controller state.
#[derive(Default)]
pub struct ClassicPeState {
    pub config: ClassicPeConfig,
    pub config_loaded: bool,
    pub touches: ClassicPeTouches,
    pub editor: ClassicPeEditor,
    pub sneak_toggled: bool,
    /// Separate RightClick button toggled shield raised (when `shield_toggle` is enabled).
    pub shield_toggled: bool,
    /// Suppresses RightClick trigger while the finger that toggled `shield_toggled` off is down.
    right_click_toggled_off: bool,
    /// Short window after pressing RightClick in toggle mode during which active shield blocking
    /// latches `shield_toggled = true` (covers taps shorter than one 20 Hz game tick).
    right_click_shield_arm: Option<Instant>,
    pub more_open: bool,
    pub hidden: bool,
    pub player_list_open: bool,
    pub zoom_toggled: bool,
    zoom_press_started: Option<f64>,
    holding_use: bool,
    was_vertical: bool,
    tap_pulse: Option<((f32, f32), Instant)>,
    break_flash: Option<((f32, f32), Instant)>,
    tap_aim_until: Option<([f32; 2], Instant)>,
    /// Temporarily yields passive sneak-to-shield while a tap attack / block-use completes.
    shield_yield_until: Option<Instant>,
    last_aim_norm: Option<[f32; 2]>,
    mine_display_progress: f32,
    last_mine_progress: f32,
    last_mine_frame: Option<Instant>,
    clock: Option<Instant>,
    pending_pad: PadState,
}

impl ClassicPeState {
    pub fn ensure_loaded(&mut self, home: &Path) {
        if !self.config_loaded {
            self.config_loaded = true;
            self.config = ClassicPeConfig::load(home);
        }
    }

    pub fn save(&self, home: &Path) {
        if let Err(e) = self.config.save(home) {
            log::warn!("Classic PE touch layout: could not save: {e}");
        }
    }

    pub fn now(&mut self) -> f64 {
        self.clock
            .get_or_insert_with(Instant::now)
            .elapsed()
            .as_secs_f64()
    }

    pub fn is_sneaking(&self) -> bool {
        !self.editor.active && (self.sneak_toggled || self.touches.held(ClassicPeControl::FlyDown))
    }

    pub fn aim_norm(&self) -> Option<[f32; 2]> {
        if self.config.split_controls || self.editor.active {
            return None;
        }
        if let Some((pos, until)) = self.tap_aim_until
            && Instant::now() < until
        {
            return Some(pos);
        }
        self.last_aim_norm
    }

    pub fn zoom_active(&self) -> bool {
        !self.editor.active && (self.zoom_toggled || self.touches.held(ClassicPeControl::Zoom))
    }

    pub fn queue_use_tap(&mut self) {
        self.pending_pad.triggers[0] = 1.0;
    }

    pub fn open_editor(&mut self) {
        self.more_open = false;
        self.hidden = false;
        self.reset_active();
        self.editor.active = true;
        self.editor.selected = Some(LayoutGroup::Dpad);
        self.editor.dragging = None;
    }

    pub fn close_editor(&mut self, home: &Path) {
        self.editor.active = false;
        self.editor.dragging = None;
        self.save(home);
    }

    pub fn reset_active(&mut self) {
        let _ = self.touches.release_all();
        self.sneak_toggled = false;
        self.shield_toggled = false;
        self.right_click_toggled_off = false;
        self.right_click_shield_arm = None;
        self.holding_use = false;
        self.zoom_press_started = None;
        self.tap_pulse = None;
        self.break_flash = None;
        self.tap_aim_until = None;
        self.last_aim_norm = None;
        self.mine_display_progress = 0.0;
        self.last_mine_progress = 0.0;
        self.pending_pad = PadState::default();
    }

    /// Temporarily yields passive crouch-to-shield so a world tap (attack or place/use) can fire.
    pub fn yield_shield(&mut self, millis: u64) {
        self.shield_yield_until = Some(Instant::now() + Duration::from_millis(millis));
    }

    pub fn layout_options(
        &self,
        vertical_movement: bool,
        zoom_available: bool,
        waypoints_available: bool,
        voice_available: bool,
    ) -> LayoutOptions {
        LayoutOptions {
            more_open: self.more_open && !self.editor.active,
            hidden: self.hidden && !self.editor.active,
            vertical_movement,
            editing: self.editor.active,
            zoom_available,
            waypoints_available,
            voice_available,
        }
    }

    /// Process a gameplay touch event (in GUI pixels `at` inside viewport `size`).
    #[allow(clippy::too_many_arguments)]
    pub fn touch_event(
        &mut self,
        id: u64,
        phase: TouchPhaseKind,
        at: (f32, f32),
        size: (f32, f32),
        opts: LayoutOptions,
        attack_button_look: bool,
        home: &Path,
    ) -> TouchStepEvents {
        self.ensure_loaded(home);
        let mut ev = TouchStepEvents::default();
        if self.editor.active {
            ev.clicked = self.editor_touch(id, phase, at, size, opts, home);
            return ev;
        }
        let layout = bedrock_layout(size.0, size.1, &self.config, opts);
        let now = self.now();
        let outs = match phase {
            TouchPhaseKind::Started => self.touches.start(id, at, now, &layout),
            TouchPhaseKind::Moved => self.touches.moved(id, at, &layout, attack_button_look),
            TouchPhaseKind::Ended => self.touches.end_at(id, at),
            TouchPhaseKind::Cancelled => self.touches.cancel(id),
        };
        let norm = |p: (f32, f32)| [p.0 / size.0.max(1.0), p.1 / size.1.max(1.0)];
        for o in outs {
            match o {
                TouchOut::Press(c) => match c {
                    ClassicPeControl::Hotbar(slot) => {
                        ev.hotbar_slot = Some(usize::from(slot));
                    }
                    ClassicPeControl::Pause => {
                        ev.pause = true;
                        ev.clicked = true;
                    }
                    ClassicPeControl::Inventory => {
                        ev.inventory = true;
                        ev.clicked = true;
                    }
                    ClassicPeControl::Chat => {
                        ev.chat = true;
                        ev.clicked = true;
                    }
                    ClassicPeControl::More => {
                        self.more_open = !self.more_open;
                        ev.clicked = true;
                    }
                    ClassicPeControl::Sneak => {
                        self.sneak_toggled = !self.sneak_toggled;
                        ev.clicked = true;
                    }
                    ClassicPeControl::Jump => {
                        self.pending_pad.set(Button::South, true);
                    }
                    ClassicPeControl::LeftClick => {
                        self.pending_pad.triggers[1] = 1.0;
                    }
                    ClassicPeControl::RightClick => {
                        if self.shield_toggled {
                            self.shield_toggled = false;
                            self.right_click_shield_arm = None;
                            self.right_click_toggled_off = true;
                        } else {
                            self.right_click_toggled_off = false;
                            self.right_click_shield_arm =
                                Some(Instant::now() + Duration::from_millis(120));
                            self.pending_pad.triggers[0] = 1.0;
                        }
                    }
                    ClassicPeControl::Debug => {
                        ev.debug = true;
                        ev.clicked = true;
                    }
                    ClassicPeControl::Perspective => {
                        ev.perspective = true;
                        ev.clicked = true;
                    }
                    ClassicPeControl::PlayerList => {
                        self.player_list_open = !self.player_list_open;
                        ev.player_list_toggled = Some(self.player_list_open);
                        ev.clicked = true;
                    }
                    ClassicPeControl::Drop => {
                        ev.drop = true;
                        ev.clicked = true;
                    }
                    ClassicPeControl::ToggleSplitControls => {
                        self.config.split_controls = !self.config.split_controls;
                        self.tap_pulse = None;
                        self.break_flash = None;
                        self.tap_aim_until = None;
                        self.last_aim_norm = None;
                        self.save(home);
                        ev.clicked = true;
                    }
                    ClassicPeControl::ToggleActionButtons => {
                        self.config.show_action_buttons = !self.config.show_action_buttons;
                        if !self.config.show_action_buttons {
                            self.shield_toggled = false;
                            self.right_click_toggled_off = false;
                            self.right_click_shield_arm = None;
                        }
                        self.save(home);
                        ev.clicked = true;
                    }
                    ClassicPeControl::Hide => {
                        self.hidden = !self.hidden;
                        self.more_open = false;
                        self.reset_active();
                        ev.clicked = true;
                    }
                    ClassicPeControl::EditLayout => {
                        self.open_editor();
                        ev.clicked = true;
                    }
                    ClassicPeControl::Zoom => {
                        self.zoom_press_started = Some(now);
                        ev.clicked = true;
                    }
                    ClassicPeControl::Waypoints => {
                        ev.waypoints = true;
                        ev.clicked = true;
                    }
                    ClassicPeControl::AddWaypoint => {
                        ev.add_waypoint = true;
                        ev.clicked = true;
                    }
                    ClassicPeControl::Voice => {
                        ev.voice = true;
                        ev.clicked = true;
                    }
                    ClassicPeControl::Forward
                    | ClassicPeControl::Back
                    | ClassicPeControl::Left
                    | ClassicPeControl::Right
                    | ClassicPeControl::FlyDown => {}
                },
                TouchOut::Release(ClassicPeControl::Zoom) => {
                    if let Some(started) = self.zoom_press_started.take() {
                        if now - started <= ZOOM_TAP_WINDOW {
                            self.zoom_toggled = !self.zoom_toggled;
                        } else {
                            self.zoom_toggled = false;
                        }
                    }
                }
                TouchOut::Release(ClassicPeControl::RightClick) => {
                    self.right_click_toggled_off = false;
                }
                TouchOut::Release(_) => {}
                TouchOut::Look(dx, dy) => {
                    ev.look_gui[0] += dx;
                    ev.look_gui[1] += dy;
                }
                TouchOut::Tap(pos) => {
                    ev.tap_world = true;
                    if !self.config.split_controls {
                        let n = norm(pos);
                        let now_inst = Instant::now();
                        self.tap_pulse = Some((pos, now_inst));
                        self.tap_aim_until = Some((n, now_inst + Duration::from_millis(160)));
                        ev.tap_aim_norm = Some(n);
                    }
                }
                TouchOut::MineStart(pos) => {
                    if !self.config.split_controls {
                        self.last_aim_norm = Some(norm(pos));
                    }
                }
                TouchOut::MineEnd => {
                    self.holding_use = false;
                    self.last_aim_norm = None;
                    self.mine_display_progress = 0.0;
                    self.last_mine_progress = 0.0;
                }
            }
        }
        if !self.config.split_controls
            && let Some(pos) = self.touches.active_mine_pos()
        {
            self.last_aim_norm = Some(norm(pos));
        }
        ev
    }

    /// Advance hold timers, smooth mining circle progress, and synthesize the frame's [`PadState`].
    #[allow(clippy::too_many_arguments)]
    pub fn frame_update(
        &mut self,
        size: (f32, f32),
        raw_mine_progress: f32,
        vertical_movement: bool,
        hold_uses_item: bool,
        entity_uses: bool,
        shield_available: bool,
        shield_button: bool,
        shield_toggle: bool,
        using_shield: bool,
        using_other: bool,
        in_menu: bool,
        focused: bool,
    ) -> FrameUpdate {
        if vertical_movement && !self.was_vertical {
            self.sneak_toggled = false;
        }
        self.was_vertical = vertical_movement;

        if in_menu || self.editor.active || !focused {
            if in_menu || !focused {
                self.reset_active();
            }
            return FrameUpdate::default();
        }

        let now = self.now();
        let outs = self.touches.frame(now);
        let norm = |p: (f32, f32)| [p.0 / size.0.max(1.0), p.1 / size.1.max(1.0)];
        for o in outs {
            if let TouchOut::MineStart(pos) = o {
                self.holding_use = hold_uses_item || entity_uses;
                if !self.config.split_controls {
                    self.last_aim_norm = Some(norm(pos));
                }
            }
        }

        let mut pad = std::mem::take(&mut self.pending_pad);

        // 1. D-Pad directional movement (supports smooth diagonals + double-tap sprint).
        let mask = self.touches.dpad_mask();
        let mut sx = 0.0_f32;
        let mut sy = 0.0_f32;
        if mask & DPAD_FORWARD != 0 {
            sy += 1.0;
        }
        if mask & DPAD_BACK != 0 {
            sy -= 1.0;
        }
        if mask & DPAD_RIGHT != 0 {
            sx += 1.0;
        }
        if mask & DPAD_LEFT != 0 {
            sx -= 1.0;
        }
        let len = sx.hypot(sy).max(1.0);
        pad.left = [sx / len, sy / len];
        if self.touches.sprinting() {
            pad.set(Button::LeftStick, true);
        }

        // 2. Jump & optional dedicated Attack / Use buttons.
        if self.touches.held(ClassicPeControl::Jump) {
            pad.set(Button::South, true);
        }
        if self.touches.held(ClassicPeControl::LeftClick) {
            pad.triggers[1] = 1.0;
        }
        let right_click_held =
            self.touches.held(ClassicPeControl::RightClick) && !self.right_click_toggled_off;
        if right_click_held {
            pad.triggers[0] = 1.0;
        }

        // 3. Sneak / FlyDown, passive crouch-to-shield & toggleable separate RightClick shield:
        // - Without separate Attack/Use buttons (`!show_action_buttons`), Sneak always raises
        //   the shield (regardless of `shield_button`), because there is no Use button on screen.
        // - With separate Attack/Use buttons (`show_action_buttons`), Horizon's Touch Controls
        //   setting (`shield_button`: Button vs Sneak) decides whether Sneak raises the shield,
        //   and `shield_toggle` (Toggle vs Hold, default Toggle) decides whether tapping the
        //   separate RightClick button latches the shield raised until tapped again.
        // - Whenever the player actively attacks, mines, or uses/places, shield yields
        //   immediately so interactions are never blocked.
        let sneaking = self.is_sneaking();
        let mining_pos = self.touches.active_mine_pos();
        let sneak_raises_shield = !self.config.show_action_buttons || !shield_button;
        let action_yielding = self.touches.held(ClassicPeControl::LeftClick)
            || mining_pos.is_some()
            || self.shield_yield_until.is_some_and(|u| Instant::now() < u);
        let sneak_shield = sneak_raises_shield
            && sneaking
            && shield_available
            && !vertical_movement
            && !action_yielding
            && !right_click_held;

        if !shield_toggle || !shield_available || !self.config.show_action_buttons || using_other {
            self.shield_toggled = false;
            self.right_click_shield_arm = None;
        } else if hold_uses_item || entity_uses || sneak_shield {
            self.right_click_shield_arm = None;
        } else if let Some(until) = self.right_click_shield_arm {
            if using_shield {
                self.shield_toggled = true;
                self.right_click_shield_arm = None;
            } else if Instant::now() >= until {
                self.right_click_shield_arm = None;
            } else {
                // Keep use held across one game tick so a quick tap on RightClick still raises
                // the shield and latches it.
                pad.triggers[0] = 1.0;
            }
        }

        let toggled_shield = self.shield_toggled
            && shield_toggle
            && shield_available
            && self.config.show_action_buttons
            && !action_yielding;
        let active_shield = sneak_shield || toggled_shield;
        if active_shield {
            pad.triggers[0] = 1.0;
        }

        // 4. World hold (mining or hold-to-use) + Touch-to-Interact aim point.
        if let Some(pos) = mining_pos {
            if !self.holding_use && (hold_uses_item || entity_uses) {
                self.holding_use = true;
            }
            if self.holding_use {
                pad.triggers[0] = 1.0;
            } else {
                pad.triggers[1] = 1.0;
            }
            if !self.config.split_controls {
                self.last_aim_norm = Some(norm(pos));
            }
        } else {
            self.holding_use = false;
            if !self.config.split_controls {
                if let Some((pos, _)) = self.touches.pending_hold_pos(now) {
                    self.last_aim_norm = Some(norm(pos));
                } else {
                    self.last_aim_norm = None;
                }
            } else {
                self.last_aim_norm = None;
            }
        }

        // 5. Smooth 60 FPS mining progress & break-completion flash.
        self.update_mine_progress(raw_mine_progress, mining_pos);

        FrameUpdate {
            pad,
            sneaking,
            sneak_shield: active_shield,
            zoom_active: self.zoom_active(),
            aim_norm: self.aim_norm(),
            holding_use: self.holding_use,
            clicked: false,
        }
    }

    fn update_mine_progress(&mut self, raw: f32, active_mine_pos: Option<(f32, f32)>) {
        let now = Instant::now();
        let dt = self
            .last_mine_frame
            .replace(now)
            .map_or(1.0 / 60.0, |prev| {
                prev.elapsed().as_secs_f32().clamp(0.0, 0.1)
            });
        let raw = raw.clamp(0.0, 1.0);
        if raw > 0.0 {
            if raw < self.last_mine_progress - 0.25 {
                self.mine_display_progress = raw;
            } else {
                let k = (dt * 24.0).clamp(0.0, 1.0);
                self.mine_display_progress += (raw - self.mine_display_progress) * k;
            }
        } else {
            if self.last_mine_progress > 0.35
                && !self.holding_use
                && let Some(pos) = active_mine_pos
            {
                self.break_flash = Some((pos, now));
            }
            self.mine_display_progress = 0.0;
        }
        self.last_mine_progress = raw;
    }

    fn editor_touch(
        &mut self,
        id: u64,
        phase: TouchPhaseKind,
        at: (f32, f32),
        size: (f32, f32),
        opts: LayoutOptions,
        home: &Path,
    ) -> bool {
        let (w, h) = size;
        let layout = bedrock_layout(w, h, &self.config, opts);
        let selected_top = self.editor.selected.is_some_and(LayoutGroup::is_top_region);
        match phase {
            TouchPhaseKind::Started => {
                for (action, _, rect) in editor_toolbar(w, h, selected_top) {
                    if contains(rect, at) {
                        match action {
                            EditorAction::SizeDown | EditorAction::SizeUp => {
                                let group = self.editor.selected.unwrap_or(LayoutGroup::Dpad);
                                self.editor.selected = Some(group);
                                let delta = if action == EditorAction::SizeUp {
                                    0.10
                                } else {
                                    -0.10
                                };
                                let current = self.config.group(group).or_else(|| {
                                    group_bounds(&layout, group).map(|b| GroupPlacement {
                                        x: (b[0] + b[2] / 2.0) / w.max(1.0),
                                        y: (b[1] + b[3] / 2.0) / h.max(1.0),
                                        scale: 1.0,
                                    })
                                });
                                if let Some(mut p) = current {
                                    p.scale = (p.scale + delta).clamp(0.6, 1.6);
                                    self.config.set_group(group, p);
                                    self.save(home);
                                }
                            }
                            EditorAction::OpacityDown | EditorAction::OpacityUp => {
                                let delta = if action == EditorAction::OpacityUp {
                                    0.10
                                } else {
                                    -0.10
                                };
                                self.config.opacity =
                                    (self.config.opacity + delta).clamp(0.25, 1.0);
                                self.save(home);
                            }
                            EditorAction::Reset => {
                                self.config.reset();
                                self.save(home);
                            }
                            EditorAction::Done => {
                                self.close_editor(home);
                            }
                        }
                        return true;
                    }
                }
                for group in LayoutGroup::ALL {
                    if let Some(b) = group_bounds(&layout, group) {
                        let pad = 4.0;
                        let hit_box = [b[0] - pad, b[1] - pad, b[2] + 2.0 * pad, b[3] + 2.0 * pad];
                        if contains(hit_box, at) {
                            let cx = b[0] + b[2] / 2.0;
                            let cy = b[1] + b[3] / 2.0;
                            self.editor.selected = Some(group);
                            self.editor.dragging = Some((id, group, (at.0 - cx, at.1 - cy)));
                            return true;
                        }
                    }
                }
                false
            }
            TouchPhaseKind::Moved => {
                if let Some((drag_id, group, offset)) = self.editor.dragging
                    && drag_id == id
                {
                    let cx = (at.0 - offset.0) / w.max(1.0);
                    let cy = (at.1 - offset.1) / h.max(1.0);
                    let scale = self.config.scale_of(group);
                    self.config.set_group(
                        group,
                        GroupPlacement {
                            x: cx.clamp(0.0, 1.0),
                            y: cy.clamp(0.0, 1.0),
                            scale,
                        },
                    );
                }
                false
            }
            TouchPhaseKind::Ended | TouchPhaseKind::Cancelled => {
                if let Some((drag_id, _, _)) = self.editor.dragging
                    && drag_id == id
                {
                    self.editor.dragging = None;
                    self.save(home);
                }
                false
            }
        }
    }

    /// Draw the classic Bedrock / Pocket Edition HUD controls, touch circle, and editor overlay.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &self,
        batch: &mut HudBatch,
        atlas: &HudAtlas,
        opts: LayoutOptions,
        zoom_active: bool,
        voice_muted: bool,
        show_touch_circle: bool,
    ) {
        let layout = bedrock_layout(batch.width, batch.height, &self.config, opts);
        let editing = self.editor.active;
        let base_opacity = self.config.opacity.clamp(0.25, 1.0);
        let dpad_mask = self.touches.dpad_mask();
        let sprinting = self.touches.sprinting();

        if editing {
            batch.fill(
                atlas,
                0.0,
                0.0,
                batch.width,
                batch.height,
                rgba(0, 0, 0, 95),
            );
            let grid_color = rgba(255, 255, 255, 22);
            for i in 1..10 {
                let gx = (batch.width * i as f32 / 10.0).floor();
                let gy = (batch.height * i as f32 / 10.0).floor();
                batch.fill(atlas, gx, 0.0, 1.0, batch.height, grid_color);
                batch.fill(atlas, 0.0, gy, batch.width, 1.0, grid_color);
            }
        }

        // Subtle diagonal D-pad corner pads while dragging the D-pad.
        if !editing
            && dpad_mask != 0
            && let Some(db) = group_bounds(&layout, LayoutGroup::Dpad)
        {
            let cell = db[2] / 3.0;
            let corners = [
                (
                    db[0],
                    db[1],
                    (dpad_mask & DPAD_FORWARD != 0) && (dpad_mask & DPAD_LEFT != 0),
                ),
                (
                    db[0] + 2.0 * cell,
                    db[1],
                    (dpad_mask & DPAD_FORWARD != 0) && (dpad_mask & DPAD_RIGHT != 0),
                ),
                (
                    db[0],
                    db[1] + 2.0 * cell,
                    (dpad_mask & DPAD_BACK != 0) && (dpad_mask & DPAD_LEFT != 0),
                ),
                (
                    db[0] + 2.0 * cell,
                    db[1] + 2.0 * cell,
                    (dpad_mask & DPAD_BACK != 0) && (dpad_mask & DPAD_RIGHT != 0),
                ),
            ];
            for (cx, cy, lit) in corners {
                let r = [cx, cy, cell, cell];
                let fill = if lit {
                    rgba(225, 232, 240, (145.0 * base_opacity).round() as u8)
                } else {
                    rgba(55, 55, 58, (55.0 * base_opacity).round() as u8)
                };
                batch.fill(atlas, r[0] + 1.0, r[1] + 1.0, r[2] - 2.0, r[3] - 2.0, fill);
                let border = rgba(
                    255,
                    255,
                    255,
                    (if lit { 130.0 } else { 40.0 } * base_opacity).round() as u8,
                );
                draw_rect_outline(batch, atlas, r, 1.0, border);
            }
        }

        for &(control, r) in &layout {
            if matches!(control, ClassicPeControl::Hotbar(_)) {
                continue;
            }
            let held = !editing && self.touches.held(control);
            let active = match control {
                ClassicPeControl::Sneak => self.sneak_toggled,
                ClassicPeControl::RightClick => self.shield_toggled,
                ClassicPeControl::More => self.more_open,
                ClassicPeControl::PlayerList => self.player_list_open,
                ClassicPeControl::ToggleActionButtons => self.config.show_action_buttons,
                ClassicPeControl::ToggleSplitControls => self.config.split_controls,
                ClassicPeControl::Forward => sprinting && held,
                ClassicPeControl::Zoom => zoom_active,
                ClassicPeControl::Voice => voice_muted,
                _ => false,
            };
            let mut alpha = if self.hidden && !editing && control == ClassicPeControl::Hide {
                0.38
            } else {
                base_opacity
            };
            if editing {
                let dimmed = (matches!(
                    control,
                    ClassicPeControl::LeftClick | ClassicPeControl::RightClick
                ) && !self.config.show_action_buttons)
                    || (control == ClassicPeControl::Zoom && !opts.zoom_available)
                    || (matches!(
                        control,
                        ClassicPeControl::Waypoints | ClassicPeControl::AddWaypoint
                    ) && !opts.waypoints_available)
                    || (control == ClassicPeControl::Voice && !opts.voice_available);
                if dimmed {
                    alpha *= 0.45;
                }
            }
            draw_bedrock_control(
                batch,
                atlas,
                control,
                r,
                held,
                active,
                sprinting && control == ClassicPeControl::Forward && held,
                self.hidden && !editing,
                alpha,
            );
        }

        if editing {
            self.draw_editor(batch, atlas, &layout, opts);
        } else if show_touch_circle && !self.config.split_controls {
            self.draw_touch_circle(batch, atlas);
        }
    }

    fn draw_touch_circle(&self, batch: &mut HudBatch, atlas: &HudAtlas) {
        let radius = (batch.height * 0.118).clamp(26.0, 42.0);
        let ring_w = 2.5;

        // 1. Active hold / mining circle (follows the finger smoothly across adjacent blocks).
        if let Some(pos) = self.touches.active_mine_pos() {
            draw_circle_ring(
                batch,
                atlas,
                pos,
                radius - 0.75,
                radius + ring_w + 0.75,
                rgba(0, 0, 0, 95),
            );
            draw_circle_ring(
                batch,
                atlas,
                pos,
                radius,
                radius + ring_w,
                rgba(255, 255, 255, 220),
            );
            draw_circle_disc(batch, atlas, pos, radius - 0.5, rgba(255, 255, 255, 28));

            let progress = self.mine_display_progress.clamp(0.0, 1.0);
            if progress > 0.005 {
                let inner_r = ((radius - 1.5) * progress).max(1.5);
                draw_circle_disc(batch, atlas, pos, inner_r, rgba(236, 242, 248, 178));
            }
            if let Some((flash_pos, started)) = self.break_flash {
                let elapsed = started.elapsed().as_secs_f32();
                if elapsed < 0.12 {
                    let fade = 1.0 - elapsed / 0.12;
                    draw_circle_disc(
                        batch,
                        atlas,
                        flash_pos,
                        radius - 1.0,
                        rgba(255, 255, 255, (fade * 195.0).round() as u8),
                    );
                }
            }
            return;
        }

        // 2. Pre-mine dwell fade-in (after 0.10s still hold, before HOLD triggers mining).
        let now = self.clock.map_or(0.0, |c| c.elapsed().as_secs_f64());
        if let Some((pos, frac)) = self.touches.pending_hold_pos(now) {
            let r = radius * (0.9 + 0.1 * frac);
            draw_circle_ring(
                batch,
                atlas,
                pos,
                r - 0.5,
                r + ring_w + 0.5,
                rgba(0, 0, 0, (frac * 80.0).round() as u8),
            );
            draw_circle_ring(
                batch,
                atlas,
                pos,
                r,
                r + ring_w,
                rgba(255, 255, 255, (frac * 195.0).round() as u8),
            );
            return;
        }

        // 3. Brief pulse/fade circle on short taps (place block, interact, attack entity).
        if let Some((pos, started)) = self.tap_pulse {
            let elapsed = started.elapsed().as_secs_f32();
            const PULSE_DURATION: f32 = 0.18;
            if elapsed < PULSE_DURATION {
                let t = elapsed / PULSE_DURATION;
                let fade = 1.0 - t;
                let r = radius * (0.82 + 0.18 * t);
                draw_circle_ring(
                    batch,
                    atlas,
                    pos,
                    r - 0.5,
                    r + ring_w + 0.5,
                    rgba(0, 0, 0, (fade * 75.0).round() as u8),
                );
                draw_circle_ring(
                    batch,
                    atlas,
                    pos,
                    r,
                    r + ring_w,
                    rgba(255, 255, 255, (fade * 210.0).round() as u8),
                );
                draw_circle_disc(
                    batch,
                    atlas,
                    pos,
                    r * 0.45,
                    rgba(255, 255, 255, (fade * 70.0).round() as u8),
                );
            }
        }
    }

    fn draw_editor(
        &self,
        batch: &mut HudBatch,
        atlas: &HudAtlas,
        layout: &[(ClassicPeControl, Rect)],
        opts: LayoutOptions,
    ) {
        let w = batch.width;
        let h = batch.height;
        let selected = self.editor.selected;

        for group in LayoutGroup::ALL {
            let Some(b) = group_bounds(layout, group) else {
                continue;
            };
            let is_sel = selected == Some(group);
            let pad = 2.0;
            let frame = [b[0] - pad, b[1] - pad, b[2] + 2.0 * pad, b[3] + 2.0 * pad];
            let border = if is_sel {
                rgba(105, 225, 255, 245)
            } else {
                rgba(255, 255, 255, 130)
            };
            draw_rect_outline(batch, atlas, frame, if is_sel { 1.5 } else { 1.0 }, border);

            let is_off = match group {
                LayoutGroup::Attack | LayoutGroup::Use => !self.config.show_action_buttons,
                LayoutGroup::Zoom => !opts.zoom_available,
                LayoutGroup::Waypoints => !opts.waypoints_available,
                LayoutGroup::Voice => !opts.voice_available,
                _ => false,
            };
            if is_sel || is_off {
                let pct = (self.config.scale_of(group) * 100.0).round() as u32;
                let tag = if is_sel && is_off {
                    format!("{} {pct}% (off)", group.label())
                } else if is_sel {
                    format!("{} {pct}%", group.label())
                } else {
                    format!("{} (off)", group.label())
                };
                let tw = HudBatch::text_width(atlas, &tag) + 6.0;
                let th = 12.0;
                let tx = (frame[0] + (frame[2] - tw) / 2.0).clamp(2.0, (w - tw - 2.0).max(2.0));
                let ty = if frame[1] >= th + 3.0 {
                    frame[1] - th - 2.0
                } else {
                    (frame[1] + frame[3] + 2.0).min((h - th - 2.0).max(2.0))
                };
                batch.fill(atlas, tx, ty, tw, th, rgba(16, 20, 26, 215));
                draw_rect_outline(batch, atlas, [tx, ty, tw, th], 1.0, border);
                batch.text(
                    atlas,
                    &tag,
                    tx + 3.0,
                    ty + 2.0,
                    if is_sel {
                        rgba(235, 250, 255, 255)
                    } else {
                        rgba(200, 205, 212, 220)
                    },
                    true,
                );
            }
        }

        let selected_top = selected.is_some_and(LayoutGroup::is_top_region);
        let buttons = editor_toolbar(w, h, selected_top);
        if let (Some(first), Some(last)) = (buttons.first(), buttons.last()) {
            let bar_x = first.2[0] - 4.0;
            let bar_y = first.2[1] - 4.0;
            let bar_w = (last.2[0] + last.2[2]) - first.2[0] + 8.0;
            let bar_h = first.2[3] + 8.0;
            batch.fill(atlas, bar_x, bar_y, bar_w, bar_h, rgba(16, 18, 22, 225));
            draw_rect_outline(
                batch,
                atlas,
                [bar_x, bar_y, bar_w, bar_h],
                1.0,
                rgba(255, 255, 255, 110),
            );

            let op_pct = (self.config.opacity * 100.0).round() as u32;
            let hint = format!("Drag controls · Opacity {op_pct}%");
            let hw = HudBatch::text_width(atlas, &hint) + 8.0;
            let hx = ((w - hw) / 2.0).clamp(2.0, (w - hw - 2.0).max(2.0));
            let hy = if selected_top {
                (bar_y - 14.0).max(2.0)
            } else {
                bar_y + bar_h + 2.0
            };
            batch.fill(atlas, hx, hy, hw, 12.0, rgba(16, 18, 22, 195));
            batch.text(
                atlas,
                &hint,
                hx + 4.0,
                hy + 2.0,
                rgba(220, 228, 236, 240),
                true,
            );
        }

        for (action, label, r) in buttons {
            let fill = match action {
                EditorAction::Done => rgba(54, 148, 76, 235),
                EditorAction::Reset => rgba(152, 64, 58, 225),
                _ => rgba(56, 62, 72, 230),
            };
            let border = match action {
                EditorAction::Done => rgba(130, 238, 152, 240),
                EditorAction::Reset => rgba(235, 138, 132, 220),
                _ => rgba(190, 198, 210, 180),
            };
            batch.fill(atlas, r[0], r[1], r[2], r[3], fill);
            draw_rect_outline(batch, atlas, r, 1.0, border);
            draw_centered_label(batch, atlas, r, label, rgba(255, 255, 255, 255));
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EditorAction {
    SizeDown,
    SizeUp,
    OpacityDown,
    OpacityUp,
    Reset,
    Done,
}

fn editor_toolbar(w: f32, h: f32, selected_top: bool) -> Vec<(EditorAction, &'static str, Rect)> {
    const ITEMS: [(EditorAction, &str); 6] = [
        (EditorAction::SizeDown, "Size -"),
        (EditorAction::SizeUp, "Size +"),
        (EditorAction::OpacityDown, "Op -"),
        (EditorAction::OpacityUp, "Op +"),
        (EditorAction::Reset, "Reset"),
        (EditorAction::Done, "Done"),
    ];
    let gap = 3.0;
    let bar_w = (w - 16.0).clamp(160.0, 300.0);
    let bw = ((bar_w - gap * (ITEMS.len() as f32 - 1.0)) / ITEMS.len() as f32)
        .floor()
        .max(22.0);
    let bh = 18.0;
    let total_w = bw * ITEMS.len() as f32 + gap * (ITEMS.len() as f32 - 1.0);
    let x0 = ((w - total_w) / 2.0).floor().max(4.0);
    let y0 = if selected_top {
        (h - 52.0).max(18.0)
    } else {
        6.0
    };
    ITEMS
        .into_iter()
        .enumerate()
        .map(|(i, (action, label))| (action, label, [x0 + i as f32 * (bw + gap), y0, bw, bh]))
        .collect()
}

// ---- Geometry & pixel-art drawing helpers ----

fn cell_size(r: Rect) -> f32 {
    (r[2] / 13.0).round().max(2.0)
}

fn shadowed(r: Rect, mut mark: impl FnMut(Rect, u32)) {
    let off = (cell_size(r) / 2.0).max(1.0);
    mark([r[0] + off, r[1] + off, r[2], r[3]], rgba(63, 63, 63, 230));
    mark(r, rgba(255, 255, 255, 240));
}

fn shadowed_colored(r: Rect, fg: u32, mut mark: impl FnMut(Rect, u32)) {
    let off = (cell_size(r) / 2.0).max(1.0);
    mark([r[0] + off, r[1] + off, r[2], r[3]], rgba(40, 40, 40, 230));
    mark(r, fg);
}

fn draw_circle_ring(
    batch: &mut HudBatch,
    atlas: &HudAtlas,
    center: (f32, f32),
    inner_r: f32,
    outer_r: f32,
    color: u32,
) {
    const SEGMENTS: usize = 36;
    if outer_r <= inner_r || outer_r <= 0.0 {
        return;
    }
    let ir = inner_r.max(0.0);
    let step = std::f32::consts::TAU / SEGMENTS as f32;
    for i in 0..SEGMENTS {
        let a0 = i as f32 * step;
        let a1 = (i + 1) as f32 * step;
        let (s0, c0) = a0.sin_cos();
        let (s1, c1) = a1.sin_cos();
        let corners = [
            [center.0 + c0 * ir, center.1 + s0 * ir],
            [center.0 + c0 * outer_r, center.1 + s0 * outer_r],
            [center.0 + c1 * outer_r, center.1 + s1 * outer_r],
            [center.0 + c1 * ir, center.1 + s1 * ir],
        ];
        batch.fill_quad(atlas, corners, color);
    }
}

fn draw_circle_disc(
    batch: &mut HudBatch,
    atlas: &HudAtlas,
    center: (f32, f32),
    radius: f32,
    color: u32,
) {
    draw_circle_ring(batch, atlas, center, 0.0, radius, color);
}

fn draw_rect_outline(batch: &mut HudBatch, atlas: &HudAtlas, r: Rect, t: f32, color: u32) {
    batch.fill(atlas, r[0], r[1], r[2], t, color);
    batch.fill(atlas, r[0], r[1] + r[3] - t, r[2], t, color);
    batch.fill(atlas, r[0], r[1] + t, t, (r[3] - 2.0 * t).max(0.0), color);
    batch.fill(
        atlas,
        r[0] + r[2] - t,
        r[1] + t,
        t,
        (r[3] - 2.0 * t).max(0.0),
        color,
    );
}

fn draw_centered_label(batch: &mut HudBatch, atlas: &HudAtlas, r: Rect, label: &str, color: u32) {
    let raw_w = HudBatch::text_width(atlas, label).max(1.0);
    let raw_h = 8.0;
    let pad_x = (r[2] * 0.14).max(2.0);
    let pad_y = (r[3] * 0.18).max(2.0);
    let max_w = (r[2] - 2.0 * pad_x).max(4.0);
    let max_h = (r[3] - 2.0 * pad_y).max(4.0);
    let scale = (max_w / raw_w).min(max_h / raw_h).clamp(0.45, 1.15);
    let cx = r[0] + r[2] / 2.0;
    let cy = r[1] + r[3] / 2.0;
    let start = batch.vertices.len();
    batch.text(
        atlas,
        label,
        (cx - raw_w / 2.0).floor(),
        (cy - raw_h / 2.0).floor(),
        color,
        true,
    );
    if (scale - 1.0).abs() > 0.02 {
        batch.scale_since(start, cx, cy, scale, scale);
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_bedrock_control(
    batch: &mut HudBatch,
    atlas: &HudAtlas,
    c: ClassicPeControl,
    r: Rect,
    held: bool,
    active: bool,
    sprinting: bool,
    hidden_mode: bool,
    opacity: f32,
) {
    let op = opacity.clamp(0.15, 1.0);
    let alpha = |base: f32| (base * op).round().clamp(0.0, 255.0) as u8;

    // 1. Classic Bedrock / PE translucent beveled tile.
    if c == ClassicPeControl::Inventory {
        let bg = if held {
            rgba(180, 188, 198, alpha(195.0))
        } else {
            rgba(22, 22, 24, alpha(175.0))
        };
        batch.fill(atlas, r[0], r[1], r[2], r[3], bg);
        draw_rect_outline(batch, atlas, r, 1.0, rgba(12, 12, 14, alpha(225.0)));
        draw_rect_outline(
            batch,
            atlas,
            [
                r[0] + 1.0,
                r[1] + 1.0,
                (r[2] - 2.0).max(0.0),
                (r[3] - 2.0).max(0.0),
            ],
            1.0,
            rgba(135, 135, 140, alpha(200.0)),
        );
    } else {
        let bg = if sprinting {
            rgba(235, 212, 130, alpha(185.0))
        } else if c == ClassicPeControl::Voice && active {
            rgba(135, 48, 48, alpha(175.0))
        } else if c == ClassicPeControl::Zoom && active {
            rgba(135, 114, 48, alpha(175.0))
        } else if held {
            rgba(215, 222, 232, alpha(175.0))
        } else if active {
            rgba(105, 178, 122, alpha(175.0))
        } else {
            rgba(54, 56, 60, alpha(150.0))
        };
        batch.fill(
            atlas,
            r[0] + 1.0,
            r[1] + 1.0,
            (r[2] - 2.0).max(0.0),
            (r[3] - 2.0).max(0.0),
            bg,
        );
        draw_rect_outline(batch, atlas, r, 1.0, rgba(16, 16, 18, alpha(205.0)));
        let hi = rgba(
            255,
            255,
            255,
            alpha(if held || active { 145.0 } else { 75.0 }),
        );
        let lo = rgba(0, 0, 0, alpha(90.0));
        if r[2] > 4.0 && r[3] > 4.0 {
            batch.fill(atlas, r[0] + 1.0, r[1] + 1.0, r[2] - 2.0, 1.0, hi);
            batch.fill(atlas, r[0] + 1.0, r[1] + 2.0, 1.0, r[3] - 3.0, hi);
            batch.fill(atlas, r[0] + 1.0, r[1] + r[3] - 2.0, r[2] - 2.0, 1.0, lo);
            batch.fill(atlas, r[0] + r[2] - 2.0, r[1] + 1.0, 1.0, r[3] - 3.0, lo);
        }
    }

    // 2. Crisp pixel-art icon or bitmap label, scaled cleanly by the same opacity.
    let fg_start = batch.vertices.len();
    match c {
        ClassicPeControl::Forward => draw_arrow_icon(batch, atlas, r, 0),
        ClassicPeControl::Back => draw_arrow_icon(batch, atlas, r, 1),
        ClassicPeControl::Left => draw_arrow_icon(batch, atlas, r, 2),
        ClassicPeControl::Right => draw_arrow_icon(batch, atlas, r, 3),
        ClassicPeControl::Sneak => draw_sneak_icon(batch, atlas, r, active),
        ClassicPeControl::Jump => draw_jump_icon(batch, atlas, r),
        ClassicPeControl::FlyDown => draw_fly_down_icon(batch, atlas, r),
        ClassicPeControl::Pause => draw_pause_icon(batch, atlas, r),
        ClassicPeControl::Chat => draw_chat_icon(batch, atlas, r),
        ClassicPeControl::More | ClassicPeControl::Inventory => draw_dots_icon(batch, atlas, r),
        ClassicPeControl::LeftClick => draw_sword_icon(batch, atlas, r),
        ClassicPeControl::RightClick => draw_use_icon(batch, atlas, r),
        ClassicPeControl::Zoom => draw_zoom_icon(batch, atlas, r, active),
        ClassicPeControl::Waypoints => draw_waypoint_icon(batch, atlas, r, false),
        ClassicPeControl::AddWaypoint => draw_waypoint_icon(batch, atlas, r, true),
        ClassicPeControl::Voice => draw_voice_icon(batch, atlas, r, active),
        ClassicPeControl::Debug => {
            draw_centered_label(batch, atlas, r, "F3", rgba(255, 255, 255, 255))
        }
        ClassicPeControl::Perspective => {
            draw_centered_label(batch, atlas, r, "F5", rgba(255, 255, 255, 255))
        }
        ClassicPeControl::PlayerList => draw_centered_label(
            batch,
            atlas,
            r,
            if active { "TAB*" } else { "TAB" },
            rgba(255, 255, 255, 255),
        ),
        ClassicPeControl::Drop => {
            draw_centered_label(batch, atlas, r, "DROP", rgba(255, 255, 255, 255))
        }
        ClassicPeControl::ToggleActionButtons => draw_centered_label(
            batch,
            atlas,
            r,
            if active { "ATK*" } else { "ATK" },
            rgba(255, 255, 255, 255),
        ),
        ClassicPeControl::ToggleSplitControls => draw_centered_label(
            batch,
            atlas,
            r,
            if active { "SPLIT*" } else { "SPLIT" },
            rgba(255, 255, 255, 255),
        ),
        ClassicPeControl::Hide => draw_centered_label(
            batch,
            atlas,
            r,
            if hidden_mode { "SHOW" } else { "HIDE" },
            rgba(255, 255, 255, 255),
        ),
        ClassicPeControl::EditLayout => {
            draw_centered_label(batch, atlas, r, "EDIT", rgba(255, 255, 255, 255))
        }
        ClassicPeControl::Hotbar(_) => {}
    }

    let fg_op = (op * 0.85 + 0.15).clamp(0.25, 1.0);
    for v in &mut batch.vertices[fg_start..] {
        let a = ((v.color >> 24) as f32 * fg_op).round().clamp(0.0, 255.0) as u32;
        v.color = (v.color & 0x00ff_ffff) | (a << 24);
    }
}

fn pixel_glyph(b: &mut HudBatch, a: &HudAtlas, [x, y, w, h]: Rect, rows: &[&str], color: u32) {
    let size = rows.len().max(1) as f32;
    let cell = w.min(h) / size;
    let (left, top) = (x + (w - cell * size) / 2.0, y + (h - cell * size) / 2.0);
    for (row, line) in rows.iter().enumerate() {
        let cells = line.as_bytes();
        let mut col = 0;
        while col < cells.len() {
            if cells[col] != b'#' {
                col += 1;
                continue;
            }
            let start = col;
            while col < cells.len() && cells[col] == b'#' {
                col += 1;
            }
            b.fill(
                a,
                left + start as f32 * cell,
                top + row as f32 * cell,
                (col - start) as f32 * cell,
                cell,
                color,
            );
        }
    }
}

/// Direction: 0 = Up, 1 = Down, 2 = Left, 3 = Right.
fn draw_arrow_icon(batch: &mut HudBatch, atlas: &HudAtlas, r: Rect, dir: u8) {
    shadowed(r, |r, color| {
        let u = (r[2].min(r[3]) / 12.0).max(1.0);
        let cx = (r[0] + r[2] / 2.0).floor();
        let cy = (r[1] + r[3] / 2.0).floor();
        for step in 0..4 {
            let s = step as f32;
            let span = (2.0 * s + 1.0) * u;
            match dir {
                0 => batch.fill(atlas, cx - span / 2.0, cy - 2.0 * u + s * u, span, u, color),
                1 => batch.fill(atlas, cx - span / 2.0, cy + 1.0 * u - s * u, span, u, color),
                2 => batch.fill(atlas, cx - 2.0 * u + s * u, cy - span / 2.0, u, span, color),
                _ => batch.fill(atlas, cx + 1.0 * u - s * u, cy - span / 2.0, u, span, color),
            }
        }
    });
}

fn draw_sneak_icon(batch: &mut HudBatch, atlas: &HudAtlas, r: Rect, toggled: bool) {
    shadowed(r, |r, color| {
        let u = (r[2].min(r[3]) / 12.0).max(1.0);
        let cx = (r[0] + r[2] / 2.0).floor();
        let cy = (r[1] + r[3] / 2.0).floor();
        let outer = 5.0 * u;
        batch.fill(atlas, cx - outer / 2.0, cy - outer / 2.0, outer, u, color);
        batch.fill(
            atlas,
            cx - outer / 2.0,
            cy + outer / 2.0 - u,
            outer,
            u,
            color,
        );
        batch.fill(atlas, cx - outer / 2.0, cy - outer / 2.0, u, outer, color);
        batch.fill(
            atlas,
            cx + outer / 2.0 - u,
            cy - outer / 2.0,
            u,
            outer,
            color,
        );
        if toggled {
            batch.fill(atlas, cx - 1.5 * u, cy - 1.5 * u, 3.0 * u, 3.0 * u, color);
        } else {
            batch.fill(atlas, cx - 0.5 * u, cy - 0.5 * u, u, u, color);
        }
    });
}

fn draw_jump_icon(batch: &mut HudBatch, atlas: &HudAtlas, r: Rect) {
    shadowed(r, |r, color| {
        let u = (r[2].min(r[3]) / 13.0).max(1.0);
        let cx = (r[0] + r[2] / 2.0).floor();
        let cy = (r[1] + r[3] / 2.0).floor();
        for step in 0..4 {
            let s = step as f32;
            let span = (2.0 * s + 1.0) * u;
            batch.fill(atlas, cx - span / 2.0, cy - 3.0 * u + s * u, span, u, color);
        }
        batch.fill(atlas, cx - 2.5 * u, cy + 2.0 * u, 5.0 * u, u, color);
    });
}

fn draw_fly_down_icon(batch: &mut HudBatch, atlas: &HudAtlas, r: Rect) {
    shadowed(r, |r, color| {
        let u = (r[2].min(r[3]) / 13.0).max(1.0);
        let cx = (r[0] + r[2] / 2.0).floor();
        let cy = (r[1] + r[3] / 2.0).floor();
        for offset in [-1.5 * u, 1.5 * u] {
            for step in 0..3 {
                let s = step as f32;
                let span = (2.0 * (2.0 - s) + 1.0) * u;
                batch.fill(
                    atlas,
                    cx - span / 2.0,
                    cy + offset - u + s * u,
                    span,
                    u,
                    color,
                );
            }
        }
    });
}

fn draw_pause_icon(batch: &mut HudBatch, atlas: &HudAtlas, r: Rect) {
    shadowed(r, |r, color| {
        let u = (r[2].min(r[3]) / 10.0).max(1.0);
        let cx = (r[0] + r[2] / 2.0).floor();
        let cy = (r[1] + r[3] / 2.0).floor();
        batch.fill(atlas, cx - 2.5 * u, cy - 2.5 * u, 1.8 * u, 5.0 * u, color);
        batch.fill(atlas, cx + 0.7 * u, cy - 2.5 * u, 1.8 * u, 5.0 * u, color);
    });
}

fn draw_chat_icon(batch: &mut HudBatch, atlas: &HudAtlas, r: Rect) {
    shadowed(r, |r, color| {
        let u = (r[2].min(r[3]) / 11.0).max(1.0);
        let cx = (r[0] + r[2] / 2.0).floor();
        let cy = (r[1] + r[3] / 2.0).floor();
        batch.fill(atlas, cx - 3.0 * u, cy - 2.5 * u, 6.0 * u, u, color);
        batch.fill(atlas, cx - 3.0 * u, cy + 0.5 * u, 6.0 * u, u, color);
        batch.fill(atlas, cx - 3.0 * u, cy - 2.5 * u, u, 4.0 * u, color);
        batch.fill(atlas, cx + 2.0 * u, cy - 2.5 * u, u, 4.0 * u, color);
        batch.fill(atlas, cx - 2.0 * u, cy + 1.5 * u, 1.5 * u, u, color);
    });
}

fn draw_dots_icon(batch: &mut HudBatch, atlas: &HudAtlas, r: Rect) {
    shadowed(r, |r, color| {
        let u = (r[2].min(r[3]) / 9.0).max(1.5);
        let cx = (r[0] + r[2] / 2.0).floor();
        let cy = (r[1] + r[3] / 2.0).floor();
        for i in [-1.0_f32, 0.0, 1.0] {
            batch.fill(
                atlas,
                cx + i * 2.2 * u - 0.6 * u,
                cy - 0.6 * u,
                1.2 * u,
                1.2 * u,
                color,
            );
        }
    });
}

fn draw_sword_icon(batch: &mut HudBatch, atlas: &HudAtlas, r: Rect) {
    shadowed(r, |r, color| {
        let u = (r[2].min(r[3]) / 12.0).max(1.0);
        let cx = (r[0] + r[2] / 2.0).floor();
        let cy = (r[1] + r[3] / 2.0).floor();
        for i in -2..=3 {
            let d = i as f32 * u;
            batch.fill(
                atlas,
                cx + d - 0.5 * u,
                cy - d - 0.5 * u,
                1.2 * u,
                1.2 * u,
                color,
            );
        }
        batch.fill(atlas, cx - 2.2 * u, cy + 0.2 * u, 1.2 * u, 1.2 * u, color);
        batch.fill(atlas, cx - 0.2 * u, cy + 2.2 * u, 1.2 * u, 1.2 * u, color);
    });
}

fn draw_use_icon(batch: &mut HudBatch, atlas: &HudAtlas, r: Rect) {
    shadowed(r, |r, color| {
        let u = (r[2].min(r[3]) / 12.0).max(1.0);
        let cx = (r[0] + r[2] / 2.0).floor();
        let cy = (r[1] + r[3] / 2.0).floor();
        batch.fill(atlas, cx - 2.5 * u, cy - 2.5 * u, 5.0 * u, u, color);
        batch.fill(atlas, cx - 2.5 * u, cy + 1.5 * u, 5.0 * u, u, color);
        batch.fill(atlas, cx - 2.5 * u, cy - 2.5 * u, u, 5.0 * u, color);
        batch.fill(atlas, cx + 1.5 * u, cy - 2.5 * u, u, 5.0 * u, color);
        batch.fill(atlas, cx - 0.5 * u, cy - 0.5 * u, u, u, color);
    });
}

fn draw_zoom_icon(batch: &mut HudBatch, atlas: &HudAtlas, r: Rect, active: bool) {
    const GLASS: [&str; 12] = [
        "...####.....",
        "..#....#....",
        ".#......#...",
        ".#......#...",
        ".#......#...",
        ".#......#...",
        "..#....#....",
        "...####.#...",
        "........##..",
        ".........##.",
        "..........##",
        "...........#",
    ];
    let fg = if active {
        rgba(255, 208, 64, 245)
    } else {
        rgba(255, 255, 255, 240)
    };
    shadowed_colored(r, fg, |r, color| {
        let inner = [r[0] + r[2] * 0.2, r[1] + r[3] * 0.2, r[2] * 0.6, r[3] * 0.6];
        pixel_glyph(batch, atlas, inner, &GLASS, color);
    });
}

fn draw_waypoint_icon(batch: &mut HudBatch, atlas: &HudAtlas, r: Rect, add: bool) {
    const PIN: [&str; 12] = [
        "....####....",
        "...######...",
        "..###..###..",
        "..##....##..",
        "..##....##..",
        "..###..###..",
        "...######...",
        "...######...",
        "....####....",
        "....####....",
        ".....##.....",
        ".....##.....",
    ];
    const LINES: [&str; 5] = ["#####", ".....", "#####", ".....", "#####"];
    const PLUS: [&str; 5] = ["..#..", "..#..", "#####", "..#..", "..#.."];
    shadowed(r, |r, color| {
        let pin = [
            r[0] + r[2] * 0.14,
            r[1] + r[3] * 0.2,
            r[2] * 0.56,
            r[3] * 0.56,
        ];
        pixel_glyph(batch, atlas, pin, &PIN, color);
        let mark = [
            r[0] + r[2] * 0.60,
            r[1] + r[3] * 0.52,
            r[2] * 0.24,
            r[3] * 0.24,
        ];
        pixel_glyph(batch, atlas, mark, if add { &PLUS } else { &LINES }, color);
    });
}

fn draw_voice_icon(batch: &mut HudBatch, atlas: &HudAtlas, r: Rect, muted: bool) {
    const MIC: [&str; 12] = [
        "....####....",
        "...######...",
        "...######...",
        "...######...",
        "...######...",
        ".#.######.#.",
        ".#..####..#.",
        "..#......#..",
        "...######...",
        ".....##.....",
        ".....##.....",
        "...######...",
    ];
    const BAR: [&str; 12] = [
        "#...........",
        "##..........",
        ".##.........",
        "..##........",
        "...##.......",
        "....##......",
        ".....##.....",
        "......##....",
        ".......##...",
        "........##..",
        ".........##.",
        "..........##",
    ];
    let fg = if muted {
        rgba(240, 92, 92, 245)
    } else {
        rgba(255, 255, 255, 240)
    };
    shadowed_colored(r, fg, |r, color| {
        let inner = [r[0] + r[2] * 0.2, r[1] + r[3] * 0.2, r[2] * 0.6, r[3] * 0.6];
        pixel_glyph(batch, atlas, inner, &MIC, color);
    });
    if muted {
        shadowed(r, |r, color| {
            let inner = [r[0] + r[2] * 0.2, r[1] + r[3] * 0.2, r[2] * 0.6, r[3] * 0.6];
            pixel_glyph(batch, atlas, inner, &BAR, color);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn center(layout: &[(ClassicPeControl, Rect)], control: ClassicPeControl) -> (f32, f32) {
        let r = layout.iter().find(|(c, _)| *c == control).unwrap().1;
        (r[0] + r[2] / 2.0, r[1] + r[3] / 2.0)
    }

    #[test]
    fn default_layout_with_all_mod_and_voice_buttons_never_overlaps() {
        for show_action_buttons in [false, true] {
            for vertical_movement in [false, true] {
                let opts = LayoutOptions {
                    more_open: false,
                    hidden: false,
                    vertical_movement,
                    editing: false,
                    zoom_available: true,
                    waypoints_available: true,
                    voice_available: true,
                };
                let cfg = ClassicPeConfig {
                    show_action_buttons,
                    ..ClassicPeConfig::default()
                };
                for (w, h) in [
                    (633.0, 292.0),
                    (427.0, 240.0),
                    (480.0, 270.0),
                    (404.0, 278.0),
                    (320.0, 240.0),
                ] {
                    let l = bedrock_layout(w, h, &cfg, opts);
                    for (i, (a, ra)) in l.iter().enumerate() {
                        assert!(
                            ra[0] >= -0.01
                                && ra[1] >= -0.01
                                && ra[0] + ra[2] <= w + 0.01
                                && ra[1] + ra[3] <= h + 0.01,
                            "{a:?} off screen at {w}x{h} (actions={show_action_buttons}, vert={vertical_movement}): {ra:?}"
                        );
                        for (b, rb) in &l[i + 1..] {
                            let apart = ra[0] + ra[2] <= rb[0] + 0.01
                                || rb[0] + rb[2] <= ra[0] + 0.01
                                || ra[1] + ra[3] <= rb[1] + 0.01
                                || rb[1] + rb[3] <= ra[1] + 0.01;
                            assert!(
                                apart,
                                "{a:?} ({ra:?}) overlaps {b:?} ({rb:?}) at {w}x{h} (actions={show_action_buttons}, vert={vertical_movement})"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn shield_follows_touch_settings_and_yields_to_interactions() {
        let mut st = ClassicPeState::default();
        st.config_loaded = true;
        let size = (480.0, 270.0);
        let opts = st.layout_options(false, false, false, false);
        let layout = bedrock_layout(size.0, size.1, &st.config, opts);
        let sneak_pos = center(&layout, ClassicPeControl::Sneak);
        let home = Path::new(".");

        // Crouch on.
        st.touch_event(
            1,
            TouchPhaseKind::Started,
            sneak_pos,
            size,
            opts,
            false,
            home,
        );
        st.touch_event(1, TouchPhaseKind::Ended, sneak_pos, size, opts, false, home);

        // 1. Without ATK buttons (`show_action_buttons=false`), Sneak ALWAYS raises shield
        // regardless of `shield_button=true` (no Use button on screen).
        let upd = st.frame_update(
            size, 0.0, false, false, false, true, true, true, false, false, false, true,
        );
        assert!(upd.sneaking && upd.sneak_shield && upd.pad.triggers[0] == 1.0);

        // 2. Yielding for a tap (attack or place/use) drops passive sneak-shield immediately.
        st.yield_shield(250);
        let yielded = st.frame_update(
            size, 0.0, false, false, false, true, true, true, false, false, false, true,
        );
        assert!(yielded.sneaking && !yielded.sneak_shield && yielded.pad.triggers[0] == 0.0);
        st.shield_yield_until = None;

        // 3. With ATK buttons ON (`show_action_buttons=true`) and `shield_button=true` ("Button"),
        // Sneak does NOT raise shield (RightClick button raises shield instead).
        st.config.show_action_buttons = true;
        let separate = st.frame_update(
            size, 0.0, false, false, false, true, true, true, false, false, false, true,
        );
        assert!(separate.sneaking && !separate.sneak_shield && separate.pad.triggers[0] == 0.0);

        // 4. With ATK buttons ON and `shield_button=false` ("Sneak"), Sneak raises shield,
        // and holding LeftClick (Attack) yields the passive sneak-shield so attacks/mining work.
        let sneak_mode = st.frame_update(
            size, 0.0, false, false, false, true, false, true, false, false, false, true,
        );
        assert!(sneak_mode.sneak_shield && sneak_mode.pad.triggers[0] == 1.0);
        let layout_atk = bedrock_layout(size.0, size.1, &st.config, opts);
        let atk_pos = center(&layout_atk, ClassicPeControl::LeftClick);
        st.touch_event(2, TouchPhaseKind::Started, atk_pos, size, opts, false, home);
        let attacking = st.frame_update(
            size, 0.0, false, false, false, true, false, true, false, false, false, true,
        );
        assert!(
            !attacking.sneak_shield
                && attacking.pad.triggers[0] == 0.0
                && attacking.pad.triggers[1] == 1.0
        );
        st.touch_event(2, TouchPhaseKind::Ended, atk_pos, size, opts, false, home);
    }

    #[test]
    fn separate_right_click_shield_supports_toggle_and_hold_modes() {
        let mut st = ClassicPeState::default();
        st.config_loaded = true;
        st.config.show_action_buttons = true;
        let size = (480.0, 270.0);
        let opts = st.layout_options(false, false, false, false);
        let layout = bedrock_layout(size.0, size.1, &st.config, opts);
        let use_pos = center(&layout, ClassicPeControl::RightClick);
        let atk_pos = center(&layout, ClassicPeControl::LeftClick);
        let home = Path::new(".");

        // Toggle mode (`shield_toggle=true`): tap RightClick -> tick starts blocking (`using_shield=true`)
        // -> shield stays raised even after releasing the button.
        st.touch_event(1, TouchPhaseKind::Started, use_pos, size, opts, false, home);
        st.touch_event(1, TouchPhaseKind::Ended, use_pos, size, opts, false, home);
        // Quick tap stays armed across the first frame until the game tick raises the shield.
        let armed = st.frame_update(
            size, 0.0, false, false, false, true, true, true, false, false, false, true,
        );
        assert_eq!(armed.pad.triggers[0], 1.0);
        // Once game reports `using_shield=true`, toggle latches on.
        let latched = st.frame_update(
            size, 0.0, false, false, false, true, true, true, true, false, false, true,
        );
        assert!(st.shield_toggled && latched.sneak_shield && latched.pad.triggers[0] == 1.0);

        // Attacking yields the toggled shield immediately so swings/mining are never blocked.
        st.touch_event(2, TouchPhaseKind::Started, atk_pos, size, opts, false, home);
        let attacking = st.frame_update(
            size, 0.0, false, false, false, true, true, true, true, false, false, true,
        );
        assert!(
            !attacking.sneak_shield
                && attacking.pad.triggers[0] == 0.0
                && attacking.pad.triggers[1] == 1.0
        );
        st.touch_event(2, TouchPhaseKind::Ended, atk_pos, size, opts, false, home);

        // Second tap on RightClick toggles shield OFF immediately (does not re-raise while held).
        st.touch_event(3, TouchPhaseKind::Started, use_pos, size, opts, false, home);
        let toggled_off = st.frame_update(
            size, 0.0, false, false, false, true, true, true, true, false, false, true,
        );
        assert!(
            !st.shield_toggled && !toggled_off.sneak_shield && toggled_off.pad.triggers[0] == 0.0
        );
        st.touch_event(3, TouchPhaseKind::Ended, use_pos, size, opts, false, home);

        // Using a non-shield item (food/bow: `using_other=true`) never latches shield toggle.
        st.touch_event(4, TouchPhaseKind::Started, use_pos, size, opts, false, home);
        let _ = st.frame_update(
            size, 0.0, false, true, false, true, true, true, false, true, false, true,
        );
        st.touch_event(4, TouchPhaseKind::Ended, use_pos, size, opts, false, home);
        assert!(!st.shield_toggled);

        // Hold mode (`shield_toggle=false`): raises only while held, lowers immediately on release.
        st.touch_event(5, TouchPhaseKind::Started, use_pos, size, opts, false, home);
        let held = st.frame_update(
            size, 0.0, false, false, false, true, true, false, true, false, false, true,
        );
        assert!(!st.shield_toggled && held.pad.triggers[0] == 1.0);
        st.touch_event(5, TouchPhaseKind::Ended, use_pos, size, opts, false, home);
        let released = st.frame_update(
            size, 0.0, false, false, false, true, true, false, true, false, false, true,
        );
        assert!(!st.shield_toggled && !released.sneak_shield && released.pad.triggers[0] == 0.0);
    }

    #[test]
    fn dpad_supports_diagonals_and_double_tap_forward_sprint() {
        let l = bedrock_layout(
            640.0,
            320.0,
            &ClassicPeConfig::default(),
            LayoutOptions::default(),
        );
        let db = group_bounds(&l, LayoutGroup::Dpad).unwrap();
        let fwd = center(&l, ClassicPeControl::Forward);
        let mut t = ClassicPeTouches::default();

        // First tap on Forward -> walking.
        let out = t.start(1, fwd, 1.0, &l);
        assert!(out.contains(&TouchOut::Press(ClassicPeControl::Forward)));
        assert!(!t.sprinting());
        let _ = t.end(1);

        // Second tap within 0.35s -> sprinting!
        let _ = t.start(2, fwd, 1.2, &l);
        assert!(t.sprinting());

        // Slide to top-left diagonal corner -> Forward + Left, still sprinting.
        let top_left = (db[0] + db[2] * 0.15, db[1] + db[3] * 0.15);
        let _ = t.moved(2, top_left, &l, false);
        assert_eq!(t.dpad_mask(), DPAD_FORWARD | DPAD_LEFT);
        assert!(t.sprinting());

        // Slide to Back -> Forward released, sprint ends.
        let back = center(&l, ClassicPeControl::Back);
        let _ = t.moved(2, back, &l, false);
        assert_eq!(t.dpad_mask(), DPAD_BACK);
        assert!(!t.sprinting());
    }

    #[test]
    fn touch_interact_hold_turns_camera_while_mining() {
        let l = bedrock_layout(
            640.0,
            320.0,
            &ClassicPeConfig::default(),
            LayoutOptions::default(),
        );
        let mut t = ClassicPeTouches::default();
        let p0 = (320.0, 160.0);
        assert!(t.start(7, p0, 1.0, &l).is_empty());
        assert!(t.pending_hold_pos(1.20).is_some());
        let out = t.frame(1.40);
        assert_eq!(out, vec![TouchOut::MineStart(p0)]);

        // Moving while mining turns the camera AND keeps active_mine_pos tracking the finger.
        let p1 = (380.0, 145.0);
        let moved = t.moved(7, p1, &l, false);
        assert_eq!(moved, vec![TouchOut::Look(60.0, -15.0)]);
        assert_eq!(t.active_mine_pos(), Some(p1));
        assert_eq!(t.end_at(7, p1), vec![TouchOut::MineEnd]);
    }

    #[test]
    fn config_round_trip_and_sanitization() {
        let mut cfg = ClassicPeConfig {
            opacity: 2.5,
            show_action_buttons: true,
            split_controls: false,
            zoom: Some(GroupPlacement {
                x: -0.5,
                y: 1.5,
                scale: 9.0,
            }),
            ..Default::default()
        };
        cfg.sanitize();
        assert_eq!(cfg.opacity, 1.0);
        let z = cfg.zoom.unwrap();
        assert_eq!((z.x, z.y, z.scale), (0.0, 1.0, 1.6));

        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: ClassicPeConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cfg);
    }

    #[test]
    fn multitouch_back_while_mining_hotbar_while_running_and_per_finger_cancel() {
        let l = bedrock_layout(
            640.0,
            320.0,
            &ClassicPeConfig::default(),
            LayoutOptions::default(),
        );
        let mut t = ClassicPeTouches::default();

        // 1. Hold finger 1 on world until mining starts, then press Back with finger 2.
        let world_pt = (320.0, 140.0);
        assert!(t.start(1, world_pt, 1.0, &l).is_empty());
        assert_eq!(t.frame(1.40), vec![TouchOut::MineStart(world_pt)]);
        let back = center(&l, ClassicPeControl::Back);
        assert_eq!(
            t.start(2, back, 1.45, &l),
            vec![TouchOut::Press(ClassicPeControl::Back)]
        );
        assert_eq!(t.active_mine_pos(), Some(world_pt));
        assert_eq!(t.dpad_mask(), DPAD_BACK);

        // 2. A cancelled third touch must NOT wipe finger 1 (mining) or finger 2 (Back),
        //    and must NOT emit a spurious world tap.
        assert!(t.start(3, (400.0, 140.0), 1.50, &l).is_empty());
        assert!(t.cancel(3).is_empty());
        assert_eq!(t.active_mine_pos(), Some(world_pt));
        assert_eq!(t.dpad_mask(), DPAD_BACK);

        // 3. Running forward + tapping a hotbar slot selects the slot without dropping Forward.
        let _ = t.end(1);
        let _ = t.end(2);
        let fwd = center(&l, ClassicPeControl::Forward);
        assert_eq!(
            t.start(4, fwd, 2.0, &l),
            vec![TouchOut::Press(ClassicPeControl::Forward)]
        );
        let slot3 = center(&l, ClassicPeControl::Hotbar(3));
        assert_eq!(
            t.start(5, slot3, 2.1, &l),
            vec![TouchOut::Press(ClassicPeControl::Hotbar(3))]
        );
        assert_eq!(t.dpad_mask(), DPAD_FORWARD);
        let _ = t.end(5);
        assert_eq!(t.dpad_mask(), DPAD_FORWARD);
    }
}
