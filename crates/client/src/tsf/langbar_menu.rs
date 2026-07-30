//! The context menu behind a right-click on the language-bar item — the あ/A
//! in the notification area (issue #98).
//!
//! Windows 11's tray does not use the `ITfMenu` pathway: `InitMenu` is never
//! called, whatever `dwStyle` the item declares. Measured on a VM
//! (2026-07-30) with both `TF_LBI_STYLE_BTN_BUTTON` and
//! `TF_LBI_STYLE_BTN_MENU`, while the right-click itself arrived at `OnClick`
//! as `TF_LBI_CLK_RIGHT` in both. So the menu is ours to draw, with
//! `TrackPopupMenuEx`, inside the host application's process — the shape ATOK
//! uses. (MS-IME's menu is the shell's own and is not offered to third-party
//! text services.)
//!
//! Three things about doing that are load-bearing:
//!
//! * `TrackPopupMenuEx` **runs its own message loop**, so TSF re-enters this
//!   thread while the menu is open. Every borrow of the `RefCell` state must
//!   be dropped before it is called: a re-entrant borrow fails, and in this
//!   crate a panic unwinds into the host application.
//! * We own no window that could receive `WM_COMMAND`. `TPM_RETURNCMD` hands
//!   the selection back as a return value and `TPM_NONOTIFY` keeps the owner
//!   from being told, so no command of ours is ever posted at a host window.
//! * A popup menu captures the mouse and keyboard through its owner window,
//!   and that window must belong to the **calling thread** — a host window
//!   would be the wrong thread and the menu could fail to dismiss, which on
//!   this thread means a hung text application. So the owner is a hidden
//!   window of our own, created and destroyed around the one call.

use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, POINT, WPARAM},
        UI::WindowsAndMessaging::{
            AppendMenuW, CheckMenuRadioItem, CreatePopupMenu, CreateWindowExW, DestroyMenu,
            DestroyWindow, HMENU, MENU_ITEM_FLAGS, MF_BYCOMMAND, MF_GRAYED, MF_SEPARATOR,
            MF_STRING, PostMessageW, SetForegroundWindow, TPM_NONOTIFY, TPM_RETURNCMD,
            TPM_RIGHTBUTTON, TrackPopupMenuEx, WM_NULL, WS_EX_TOOLWINDOW, WS_POPUP,
        },
    },
    core::{PCWSTR, w},
};

use anyhow::Result;

use crate::engine::{engine_health, engine_health::EngineHealth, input_mode::InputMode};

/// Everything the menu can ask for.
///
/// The command ids live in [`id`] and [`from_id`] and nowhere else: the
/// builder and the selection handler read the same two functions, so an item
/// cannot be appended under one number and matched under another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuItem {
    /// Switch to kana input.
    Kana,
    /// Switch to half-width latin.
    Latin,
    /// Open the settings app.
    Settings,
}

/// `TrackPopupMenuEx` reports "nothing was chosen" as 0, so no item may take
/// that id.
const FIRST_ID: u32 = 1;

/// The command id `AppendMenuW` gets for `item`.
pub fn id(item: MenuItem) -> u32 {
    match item {
        // Kana and Latin are adjacent on purpose: `CheckMenuRadioItem` takes a
        // RANGE of ids, and `mode_id_range` below is that range.
        MenuItem::Kana => FIRST_ID,
        MenuItem::Latin => FIRST_ID + 1,
        MenuItem::Settings => FIRST_ID + 2,
    }
}

/// The item `TrackPopupMenuEx` returned, or `None` for an id we never handed
/// out — including 0, which is how it says the menu was dismissed. Written in
/// terms of [`id`] so the two cannot disagree.
pub fn from_id(value: u32) -> Option<MenuItem> {
    match value {
        v if v == id(MenuItem::Kana) => Some(MenuItem::Kana),
        v if v == id(MenuItem::Latin) => Some(MenuItem::Latin),
        v if v == id(MenuItem::Settings) => Some(MenuItem::Settings),
        _ => None,
    }
}

/// The id range `CheckMenuRadioItem` bullets one item within.
fn mode_id_range() -> (u32, u32) {
    (id(MenuItem::Kana), id(MenuItem::Latin))
}

/// One row of the menu: a description of what to draw, built by
/// [`menu_entries`] and turned into an `HMENU` by [`build_menu`], so the list
/// itself can be decided and tested without a desktop.
#[derive(Debug, PartialEq, Eq)]
pub enum MenuEntry {
    /// An input mode, drawn as a radio group with the current one bulleted.
    Mode {
        item: MenuItem,
        label: &'static str,
        current: bool,
    },
    /// A plain command.
    Command {
        item: MenuItem,
        label: &'static str,
    },
    Separator,
    /// Disabled: something to read, not to pick.
    Notice(&'static str),
}

/// What the menu shows for `mode`, plus whatever `health` has to say.
///
/// Only the modes that exist as persistent state are offered:
/// [`InputMode`] is the two of them, and the F7/F8 katakana forms are a
/// one-off rewrite of the current reading rather than a mode to sit in.
/// Listing 全角カタカナ and friends greyed out would promise a feature that is
/// not there.
///
/// The mnemonics (`&H`, `&A`, `&S`) are what the other IMEs' menus have, and
/// they are the only way to work this menu from the keyboard.
pub fn menu_entries(mode: &InputMode, health: EngineHealth) -> Vec<MenuEntry> {
    let mut entries = Vec::new();

    // First, because it is the reason the rest of the menu will not appear to
    // do anything (issue #79 in menu form).
    if let Some(notice) = engine_health::menu_notice(health) {
        entries.push(MenuEntry::Notice(notice));
        entries.push(MenuEntry::Separator);
    }

    entries.push(MenuEntry::Mode {
        item: MenuItem::Kana,
        label: "ひらがな(&H)",
        current: *mode == InputMode::Kana,
    });
    entries.push(MenuEntry::Mode {
        item: MenuItem::Latin,
        label: "半角英数(&A)",
        current: *mode == InputMode::Latin,
    });
    entries.push(MenuEntry::Separator);
    entries.push(MenuEntry::Command {
        item: MenuItem::Settings,
        label: "設定(&S)…",
    });

    entries
}

/// An `HMENU` that is destroyed however the caller leaves — including the `?`
/// on an `AppendMenuW` halfway through building it. A leaked menu is a leaked
/// USER handle in a host application we do not own.
struct PopupMenu(HMENU);

impl PopupMenu {
    fn create() -> Result<Self> {
        Ok(Self(unsafe { CreatePopupMenu() }?))
    }

    fn handle(&self) -> HMENU {
        self.0
    }
}

impl Drop for PopupMenu {
    fn drop(&mut self) {
        // nothing to propagate to and nothing to do about it
        if let Err(error) = unsafe { DestroyMenu(self.0) } {
            tracing::warn!("could not destroy the language-bar menu: {error:?}");
        }
    }
}

/// The hidden window the menu is owned by, destroyed the same way.
///
/// A predefined class (`STATIC`) rather than one of our own: registering a
/// window class from a DLL means unregistering it before the DLL unloads, and
/// this window exists for the length of one menu.
struct OwnerWindow(HWND);

impl OwnerWindow {
    fn create() -> Result<Self> {
        let hwnd = unsafe {
            CreateWindowExW(
                // out of the taskbar and out of Alt+Tab; it is never seen
                WS_EX_TOOLWINDOW,
                w!("STATIC"),
                PCWSTR::null(),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                None,
                None,
            )
        }?;
        Ok(Self(hwnd))
    }

    fn handle(&self) -> HWND {
        self.0
    }
}

impl Drop for OwnerWindow {
    fn drop(&mut self) {
        if let Err(error) = unsafe { DestroyWindow(self.0) } {
            tracing::warn!("could not destroy the language-bar menu's owner window: {error:?}");
        }
    }
}

/// Turns `entries` into a menu. Every string is copied into the menu by
/// `AppendMenuW`, so nothing here has to outlive the call.
fn build_menu(entries: &[MenuEntry]) -> Result<PopupMenu> {
    let menu = PopupMenu::create()?;
    let mut current_mode: Option<u32> = None;

    for entry in entries {
        match entry {
            MenuEntry::Mode {
                item,
                label,
                current,
            } => {
                append_string(menu.handle(), MF_STRING, id(*item), label)?;
                if *current {
                    current_mode = Some(id(*item));
                }
            }
            MenuEntry::Command { item, label } => {
                append_string(menu.handle(), MF_STRING, id(*item), label)?;
            }
            MenuEntry::Separator => unsafe {
                AppendMenuW(menu.handle(), MF_SEPARATOR, 0, PCWSTR::null())?;
            },
            // id 0 is the "no command" id, which is what a disabled item
            // wants: it can never come back from TrackPopupMenuEx
            MenuEntry::Notice(text) => {
                append_string(menu.handle(), MF_GRAYED, 0, text)?;
            }
        }
    }

    // In one call at the end, not per item: this is also what turns the two
    // mode items into a radio group (`MFT_RADIOCHECK`, the bullet rather than
    // the check mark), which `AppendMenuW`'s flags cannot express.
    if let Some(check) = current_mode {
        let (first, last) = mode_id_range();
        unsafe { CheckMenuRadioItem(menu.handle(), first, last, check, MF_BYCOMMAND.0) }?;
    }

    Ok(menu)
}

fn append_string(menu: HMENU, flags: MENU_ITEM_FLAGS, id: u32, label: &str) -> Result<()> {
    let wide: Vec<u16> = label.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe { AppendMenuW(menu, flags | MF_STRING, id as usize, PCWSTR(wide.as_ptr())) }?;
    Ok(())
}

/// Shows `entries` at `at` (screen coordinates, as `OnClick` reports them) and
/// returns what the user picked, or `None` if they dismissed it.
///
/// **Blocks until the menu closes** — `TrackPopupMenuEx` runs its own message
/// loop. The caller must hold no borrow of the TIP's state across this call.
pub fn show_menu(entries: &[MenuEntry], at: POINT) -> Result<Option<MenuItem>> {
    let menu = build_menu(entries)?;
    let owner = OwnerWindow::create()?;

    // The tray-menu recipe (KB135788): a menu whose owner is not the
    // foreground window can fail to notice the click that should dismiss it,
    // and this call blocks the host's UI thread until it is dismissed. The
    // return value is deliberately ignored — a refusal is not a reason to
    // skip the menu, and there is nothing better to do about it.
    let _ = unsafe { SetForegroundWindow(owner.handle()) };

    let selected = unsafe {
        TrackPopupMenuEx(
            menu.handle(),
            // RETURNCMD: take the selection as the return value, since we have
            // no window to receive WM_COMMAND. NONOTIFY: and do not send it to
            // the owner either. RIGHTBUTTON: the menu was opened with the
            // right button, so let it be used to pick with too.
            (TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON).0,
            at.x,
            at.y,
            owner.handle(),
            // alignment is left to the default: TrackPopupMenuEx keeps the
            // menu on the monitor, which is what flips it above a tray icon
            None,
        )
    };

    // The other half of KB135788: the hidden window has the foreground and
    // will not give it up until it has processed a message.
    if let Err(error) = unsafe { PostMessageW(Some(owner.handle()), WM_NULL, WPARAM(0), LPARAM(0)) }
    {
        tracing::warn!("could not post the menu's dismissal message: {error:?}");
    }

    // 0 is "dismissed", and TPM_RETURNCMD gives us no way to tell that from a
    // failure — both mean "do nothing", which is the right outcome for either.
    Ok(u32::try_from(selected.0).ok().and_then(from_id))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const EVERY_ITEM: [MenuItem; 3] = [MenuItem::Kana, MenuItem::Latin, MenuItem::Settings];

    /// The round trip is the whole contract between the two halves of the
    /// menu: build with `id`, act on `from_id`.
    #[test]
    fn every_item_survives_the_round_trip_through_its_command_id() {
        for item in EVERY_ITEM {
            assert_eq!(from_id(id(item)), Some(item));
        }
    }

    /// A duplicated id would silently make two menu items the same command.
    #[test]
    fn every_item_has_its_own_command_id() {
        let mut ids: Vec<u32> = EVERY_ITEM.iter().map(|item| id(*item)).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "two menu items share a command id");
    }

    /// `TrackPopupMenuEx` returns 0 when the menu was dismissed, so 0 must not
    /// be an item — and an id we never handed out must not panic or be guessed
    /// at, whatever the OS gives us.
    #[test]
    fn an_id_we_never_handed_out_is_not_an_item() {
        assert_eq!(from_id(0), None);
        assert_eq!(from_id(u32::MAX), None);

        let unused = EVERY_ITEM.iter().map(|item| id(*item)).max().unwrap() + 1;
        assert_eq!(from_id(unused), None);
    }

    /// `CheckMenuRadioItem` takes a range of ids and bullets one of them, so
    /// the two modes have to be the only items inside that range.
    #[test]
    fn the_radio_range_covers_the_modes_and_nothing_else() {
        let (first, last) = mode_id_range();
        assert!(first <= last);

        for item in EVERY_ITEM {
            let inside = (first..=last).contains(&id(item));
            let is_mode = matches!(item, MenuItem::Kana | MenuItem::Latin);
            assert_eq!(
                inside, is_mode,
                "{item:?} is on the wrong side of the range"
            );
        }
    }

    fn labels(entries: &[MenuEntry]) -> Vec<&'static str> {
        entries
            .iter()
            .filter_map(|entry| match entry {
                MenuEntry::Mode { label, .. } | MenuEntry::Command { label, .. } => Some(*label),
                MenuEntry::Notice(text) => Some(*text),
                MenuEntry::Separator => None,
            })
            .collect()
    }

    fn bulleted(entries: &[MenuEntry]) -> Vec<MenuItem> {
        entries
            .iter()
            .filter_map(|entry| match entry {
                MenuEntry::Mode { item, current, .. } if *current => Some(*item),
                MenuEntry::Mode { .. }
                | MenuEntry::Command { .. }
                | MenuEntry::Separator
                | MenuEntry::Notice(_) => None,
            })
            .collect()
    }

    /// Exactly one mode is bulleted, and it is the one we are in. Both modes
    /// unmarked (or both marked) is the mistake a hand-written `current` flag
    /// invites, and the menu is then wrong about the state it exists to show.
    #[test]
    fn the_mode_we_are_in_is_the_one_that_is_bulleted() {
        for (mode, expected) in [
            (InputMode::Kana, MenuItem::Kana),
            (InputMode::Latin, MenuItem::Latin),
        ] {
            let entries = menu_entries(&mode, EngineHealth::Reachable);
            assert_eq!(bulleted(&entries), vec![expected], "for {mode:?}");
        }
    }

    /// Both modes and the settings item, always.
    #[test]
    fn both_modes_and_the_settings_item_are_offered() {
        let entries = menu_entries(&InputMode::Kana, EngineHealth::Reachable);
        let items: Vec<MenuItem> = entries
            .iter()
            .filter_map(|entry| match entry {
                MenuEntry::Mode { item, .. } | MenuEntry::Command { item, .. } => Some(*item),
                MenuEntry::Separator | MenuEntry::Notice(_) => None,
            })
            .collect();

        assert_eq!(
            items,
            vec![MenuItem::Kana, MenuItem::Latin, MenuItem::Settings]
        );
    }

    /// A working engine adds nothing: the menu must not grow a line of
    /// explanation just because the feature exists.
    #[test]
    fn a_healthy_engine_adds_nothing_to_the_menu() {
        for health in [EngineHealth::Reachable, EngineHealth::Unknown] {
            let entries = menu_entries(&InputMode::Kana, health);
            assert_eq!(
                labels(&entries).len(),
                3,
                "{health:?} should add no row: {entries:?}"
            );
        }
    }

    /// ...and an engine that is not there says so, first, as the one thing
    /// that explains why picking ひらがな will not make anything convert.
    #[test]
    fn an_unreachable_engine_is_said_out_loud_at_the_top() {
        let entries = menu_entries(&InputMode::Kana, EngineHealth::Unreachable);

        assert_eq!(
            entries.first(),
            Some(&MenuEntry::Notice(
                engine_health::menu_notice(EngineHealth::Unreachable).unwrap()
            ))
        );
        assert_eq!(labels(&entries).len(), 4);
    }
}
