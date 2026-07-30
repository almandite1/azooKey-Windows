//! Static checks on the installer's configuration files.
//!
//! These are regression guards, not behavioral tests: they read the files
//! the installer ships and assert the properties that broke when the
//! install location moved to `C:\Program Files\Azookey` (a path with a
//! space) and to a machine-wide scope. Actual install/uninstall behavior
//! still needs a VM pass.
//!
//! They live in the launcher crate because the launcher is what the
//! startup task ultimately starts; there is no installer crate.

use std::path::PathBuf;

fn installer_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../installer")
}

fn read(name: &str) -> String {
    let path = installer_dir().join(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()))
}

/// Every process the launcher supervises has to be in `build/` for the
/// installer's `../build/*` to pick it up.
///
/// The failure this guards is quiet in the worst way: a supervised child
/// that was never packaged does not break the build or the install. The
/// launcher just fails to spawn it on the user's machine, and — for a
/// child supervised non-fatally, which is exactly the kind most likely to
/// be forgotten — the IME then works fine with the feature silently
/// missing.
#[test]
fn every_supervised_binary_is_packaged() {
    let makefile = workspace_file("Makefile.toml");

    // derived from main.rs rather than listed here: a hardcoded list
    // stays green when a fourth child is added and not packaged, which is
    // the whole failure this guards against
    let supervised = supervised_binaries();
    assert!(
        supervised.contains(&"plugin-host.exe".to_string()),
        "the plugin host should be among the supervised children: {supervised:?}"
    );

    for exe in &supervised {
        assert!(
            makefile.contains(&format!("cp target/$str/{exe} build")),
            "post_build must copy {exe} into build/"
        );
        assert!(
            makefile.contains(&format!("\"build/{exe}\"")),
            "{exe} must be in post_build's required-artifact list, or a \
             missing one ships as a broken installer"
        );
    }
    // launcher.exe supervises rather than being supervised, so it is not
    // in the derived list — and it still has to be packaged
    assert!(makefile.contains("cp target/$str/launcher.exe build"));
}

/// The plugin host must be spawned by the NON-fatal supervisor.
///
/// Swapping it for `run_supervisor` compiles, passes every other test,
/// and quietly converts "the add-ons are gone" into "the IME is gone" —
/// the launcher exits, and the job object takes the engine and the
/// candidate window with it. There is no runtime test that could catch
/// that without staging a crash loop, so the wiring is read instead.
#[test]
fn the_plugin_host_is_supervised_non_fatally() {
    let main = workspace_file("crates/launcher/src/main.rs");

    let call = main
        .split("supervisor::run")
        .find(|section| section.contains("plugin-host.exe"))
        .expect("main.rs should supervise plugin-host.exe");

    assert!(
        call.starts_with("_optional_supervisor("),
        "plugin-host.exe must go through run_optional_supervisor, or losing \
         it takes the whole IME down: got {call:.60}"
    );
}

/// The settings app's executable name must be the one the bundler will
/// actually produce.
///
/// Tauri names the binary after `mainBinaryName`, and WITHOUT that key it
/// names it after the crate — so `productName` being "Azookey" says
/// nothing about what ships. That is how the installer came to name a
/// process no machine has ever run (#97): the taskkill and the WMI query
/// asked for `Azookey.exe` while the file was called `frontend.exe`, and
/// every test here passed because they asked for the same wrong name.
#[test]
fn the_settings_app_is_named_the_same_everywhere() {
    let binary = settings_app_binary();
    assert_eq!(
        binary, "Azookey.exe",
        "the settings app's name changed; every reference below has to change with it"
    );

    let iss = read("Installer.iss");
    let params = iss
        .lines()
        .find(|l| l.contains("/IM") && l.contains("launcher.exe"))
        .expect("Installer.iss should pass the processes to taskkill via /IM");
    assert!(
        params.contains(&binary),
        "uninstall's taskkill must name the settings app as it is actually \
         built: got {params}"
    );

    let query = code_block(&iss, "function StackProcesses")
        .split("ExecQuery")
        .nth(1)
        .expect("StackProcesses should run a WMI query")
        .to_string();
    assert!(
        query.contains(&binary),
        "the WMI query must name the settings app as it is actually built: \
         got {query}"
    );

    assert!(
        workspace_file("SIGNING.md").contains(&binary),
        "an unsigned settings app is the failure this list exists to prevent"
    );

    // The TIP starts it too, from the language-bar menu (#98), and it is the
    // reference furthest from the bundler: a rename would leave a menu item
    // that quietly does nothing.
    assert!(
        workspace_file("crates/client/src/tsf/settings_app.rs").contains(&binary),
        "the language-bar menu must start the settings app by the name it is \
         actually built under"
    );
}

/// The settings app's old name has to be deleted AND stopped.
///
/// Tauri removes a renamed binary itself, but only once and only if the
/// file is free: upgrading with the settings app open leaves a 10 MB
/// orphan, because the running-instance check looks for the NEW name and
/// never stops the old process — and the removal is not retried, since by
/// then the registry records the new name. Both halves were reproduced on
/// a test machine, which is why Setup does it instead.
///
/// The delete is useless without the stop: a running exe cannot be
/// deleted. Nothing else would notice if one of the two were dropped.
#[test]
fn the_settings_apps_old_name_is_both_stopped_and_deleted() {
    let iss = read("Installer.iss");
    const LEGACY: &str = "frontend.exe";

    assert!(
        iss.contains(&format!("Type: files; Name: \"{{app}}\\{LEGACY}\"")),
        "the legacy binary must be deleted on install"
    );

    let query = code_block(&iss, "function StackProcesses")
        .split("ExecQuery")
        .nth(1)
        .expect("StackProcesses should run a WMI query")
        .to_string();
    assert!(
        query.contains(LEGACY),
        "the legacy binary must be stopped, or the delete above hits a \
         locked file: got {query}"
    );

    // ...and stopped by PATH, not by name: "frontend.exe" is not ours
    // wherever it happens to run
    let body = code_block(&iss, "function StackProcesses");
    let name_matched = body
        .lines()
        .filter(|l| l.contains("Name = '") || l.contains("(Name ="))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        !name_matched.to_lowercase().contains(LEGACY),
        "{LEGACY} must not be matched by name alone: got {name_matched}"
    );
}

/// Everything the installer ships as an executable has to be on the
/// signing list. `plugin-host.exe` was added, packaged, and left off it —
/// which would have shipped one unsigned binary among signed ones, the
/// hardest kind of gap to notice because nothing fails.
#[test]
fn every_packaged_executable_is_on_the_signing_list() {
    let signing = workspace_file("SIGNING.md");

    for exe in required_executables() {
        assert!(
            signing.contains(&exe),
            "{exe} is packaged but missing from SIGNING.md"
        );
    }
}

/// `mainBinaryName` from the Tauri config, with the extension the bundler
/// adds. Read rather than hardcoded — hardcoding is what let the two
/// drift apart in the first place.
fn settings_app_binary() -> String {
    let config = workspace_file("frontend/src-tauri/tauri.conf.json");
    let name = config
        .lines()
        .find_map(|line| line.trim().strip_prefix("\"mainBinaryName\":"))
        .map(|rest| rest.trim().trim_matches(|c| c == '"' || c == ',').trim())
        .map(str::to_string)
        .expect(
            "tauri.conf.json must set mainBinaryName; without it Tauri names \
             the binary after the crate and every reference to it here is wrong",
        );
    format!("{name}.exe")
}

/// The `.exe` entries of post_build's required-artifact list — what the
/// installer is guaranteed to ship.
fn required_executables() -> Vec<String> {
    workspace_file("Makefile.toml")
        .lines()
        .filter_map(|line| line.trim().strip_prefix("\"build/"))
        .filter_map(|rest| rest.split('"').next())
        .filter(|name| name.ends_with(".exe"))
        .map(str::to_string)
        .collect()
}

fn workspace_file(relative: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()))
}

/// The executable names the launcher spawns, read out of its own source.
fn supervised_binaries() -> Vec<String> {
    workspace_file("crates/launcher/src/main.rs")
        .split("supervisor::run")
        .skip(1)
        .filter_map(|section| section.split('"').nth(1).map(str::to_string))
        .collect()
}

/// The task's Arguments become `C:\Program Files\Azookey\launch.vbs` at
/// install time. wscript.exe does not do CreateProcess-style prefix
/// guessing: an unquoted path with a space makes it look for
/// `C:\Program`, and logon autostart silently dies.
#[test]
fn task_xml_quotes_the_script_path() {
    let xml = read("Azookey Startup.xml");

    assert!(
        xml.contains(r#"<Arguments>"PATH_TO_VBS"</Arguments>"#),
        "the wscript argument must be quoted, or a space in the install \
         path breaks logon autostart"
    );
}

/// The install is machine-wide (Program Files, HKLM/HKCR registrations),
/// so the logon trigger must fire for every user. A principal of
/// BUILTIN\Administrators (S-1-5-32-544) means standard users get no
/// launcher, no server, no UI — a completely dead IME.
#[test]
fn task_xml_triggers_for_all_users() {
    let xml = read("Azookey Startup.xml");

    assert!(
        !xml.contains("S-1-5-32-544"),
        "the startup task must not be restricted to Administrators"
    );
    assert!(
        xml.contains("S-1-5-32-545") || xml.contains("S-1-5-4"),
        "the startup task should trigger for Users (S-1-5-32-545) or \
         INTERACTIVE (S-1-5-4)"
    );
}

/// The launcher supervises the whole IME stack, so the startup task must
/// keep running on battery power: with StopIfGoingOnBatteries=true, Task
/// Scheduler kills the launcher (and with it server + UI, via the job
/// object) the moment a laptop is unplugged, leaving the user without an
/// IME until the next logon.
#[test]
fn task_xml_survives_switching_to_battery_power() {
    let xml = read("Azookey Startup.xml");

    assert!(
        xml.contains("<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>"),
        "the startup task must not stop when the machine goes on battery"
    );
    assert!(
        xml.contains("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"),
        "the startup task must start even when the machine is on battery"
    );
}

/// The declaration must name **no** encoding at all.
///
/// `schtasks /Create /XML` widens the file's bytes to a wide string before
/// handing it to MSXML, so the parser already knows it is looking at Unicode.
/// A declaration that then names a byte encoding contradicts that, and MSXML
/// refuses with "Switch from current encoding to specified encoding not
/// supported" — the task is never created, and the installer discards the
/// error, so the only symptom is an IME that does not start at logon.
///
/// Both halves of this have been got wrong. The file shipped as 8-bit bytes
/// declaring `UTF-16`, which reads as a lie about the bytes; "fixing" it to
/// `UTF-8` made the document self-consistent and broke the only consumer it
/// has. Measured against real `schtasks`: no declaration, `UTF-16`, and
/// UTF-16LE bytes with a `UTF-16` declaration are all accepted; 8-bit bytes
/// declaring `UTF-8` is the one combination that fails. Naming nothing is the
/// only form that is neither a lie nor a conflict, so that is what is pinned
/// here.
#[test]
fn task_xml_declares_no_encoding() {
    let path = installer_dir().join("Azookey Startup.xml");
    let bytes = std::fs::read(&path).unwrap();

    assert!(
        bytes.starts_with(b"<?xml"),
        "the task XML is expected to be 8-bit text without a BOM \
         (the installer round-trips it as AnsiString)"
    );

    let xml = String::from_utf8(bytes).expect("task XML should be valid UTF-8");
    let declaration = xml.lines().next().unwrap_or_default();
    assert!(
        !declaration.contains("encoding="),
        "the XML declaration must not name an encoding (found {declaration:?}); \
         schtasks parses this as Unicode and rejects any byte encoding named here"
    );
}

/// Issue #104: the settings app is shipped by Setup, not by a second
/// installer chained from it.
///
/// The Tauri NSIS this replaces registered its own uninstall key, which is
/// why "Azookey" appeared twice in Installed apps. Nothing about a chained
/// installer fails loudly if it comes back — the product installs and runs
/// exactly as before, with one extra ARP entry — so the two halves of the
/// arrangement are pinned here: the exe is copied into `build/` where the
/// `../build/*` glob will find it, and no NSIS setup is referenced at all.
#[test]
fn the_settings_app_is_shipped_not_chained() {
    let iss = read("Installer.iss");

    for gone in ["TauriSetupExe", "x64-setup"] {
        assert!(
            !iss.contains(gone),
            "Installer.iss must not reference the Tauri NSIS bundle any more, \
             or the duplicate Installed apps entry is back: found {gone}"
        );
    }

    let makefile = workspace_file("Makefile.toml");
    let binary = settings_app_binary();
    assert!(
        makefile.contains(&format!("cp target/release/{binary} build")),
        "post_build must copy the settings app into build/ — it is the only \
         thing that puts it in front of the installer's ../build/* glob now"
    );
    // being in the required list is also what makes
    // every_packaged_executable_is_on_the_signing_list demand it in SIGNING.md
    assert!(
        makefile.contains(&format!("\"build/{binary}\"")),
        "the settings app must be in post_build's required-artifact list, or \
         a build that quietly skipped it ships an installer without it"
    );
}

/// Issue #104: WebView2 is now Inno's job, and nothing else's.
///
/// The chained Tauri NSIS was the product's only WebView2 bootstrap. ui.exe
/// needs the runtime as much as the settings app does, so dropping the chain
/// without picking this up leaves, on a machine without WebView2, an IME that
/// starts and shows no candidate window — and says nothing about why. There is
/// no runtime test for that; this line is the whole guard.
#[test]
fn webview2_is_bootstrapped_by_inno() {
    let iss = read("Installer.iss");

    let body = code_block(&iss, "function InitializeSetup");
    assert!(
        body.contains("Dependency_AddWebView2"),
        "InitializeSetup must bootstrap WebView2 now that no installer is \
         chained to do it: got {body}"
    );
}

/// Issue #104: an upgrade has to remove what the chained NSIS left, or the
/// second Installed apps entry outlives the installer that created it.
///
/// The mechanism is read, not just the call: the entry is the point, and the
/// uninstaller and the shortcut are what would otherwise be orphaned in
/// `{app}` and on every user's desktop. The key-existence guard matters as
/// much — without it a first install would delete a same-named shortcut that
/// belongs to somebody else.
#[test]
fn upgrade_removes_the_legacy_nsis_install() {
    let iss = read("Installer.iss");

    let prepare = code_block(&iss, "function PrepareToInstall(");
    assert!(
        prepare.contains("RemoveLegacyNsisSettingsApp"),
        "the cleanup must run from PrepareToInstall, before the copies and \
         also for a silent install: got {prepare}"
    );

    let body = code_block(&iss, "procedure RemoveLegacyNsisSettingsApp");
    assert!(
        body.contains("RegKeyExists("),
        "the cleanup must be guarded on the legacy key existing, or a first \
         install deletes files that are not ours: got {body}"
    );
    assert!(
        body.contains("RegDeleteKeyIncludingSubkeys") && body.contains("LegacyNsisKey"),
        "the duplicate uninstall entry itself must be deleted — it is the \
         whole point: got {body}"
    );
    assert!(
        body.contains("RemoveQuotes("),
        "NSIS stores InstallLocation quoted, and joining onto that names a \
         path that exists nowhere: got {body}"
    );
    assert!(
        body.contains("uninstall.exe"),
        "the orphaned NSIS uninstaller must go with its entry: got {body}"
    );
    assert!(
        body.contains("commondesktop") && body.contains("Azookey.lnk"),
        "the desktop shortcut the silent chain created for all users must be \
         removed; none is created in its place: got {body}"
    );

    assert!(
        !iss.contains("UninstallAzookey"),
        "the uninstall-time chain into the NSIS uninstaller must be gone: \
         there is no longer a second installer to uninstall"
    );
}

/// Issue #104: the start-menu shortcut has to survive the migration.
///
/// The old NSIS created a top-level all-users `Azookey.lnk`; `{autoprograms}`
/// on an admin install is that same folder. The shortcut is therefore
/// overwritten in place rather than duplicated — the SAME NAME is the
/// migration, so it is what this asserts. Get it wrong and the upgrade leaves
/// an orphan pointing at a file that may no longer be there.
#[test]
fn the_settings_app_gets_a_start_menu_shortcut() {
    let iss = read("Installer.iss");

    let section: String = iss
        .split_once("\n[Icons]")
        .expect("Installer.iss should have an [Icons] section")
        .1
        .lines()
        .take_while(|l| !l.starts_with('['))
        .collect::<Vec<_>>()
        .join("\n");

    let entry = section
        .lines()
        .find(|l| l.starts_with("Name:"))
        .unwrap_or_else(|| panic!("[Icons] must define a shortcut: got {section}"));

    assert!(
        entry.contains(r"{autoprograms}\"),
        "the shortcut must land in the common Programs folder, where the old \
         NSIS put its own: got {entry}"
    );
    assert!(
        entry.contains(r#"Filename: "{app}\Azookey.exe""#),
        "the shortcut must point at the settings app Setup installs: got {entry}"
    );
    // the .lnk is named after the Name: entry's last component
    assert!(
        entry.contains(r"{autoprograms}\{#MyAppName}"),
        "the shortcut has to keep the old NSIS name (Azookey.lnk) — that is \
         what makes the upgrade overwrite it instead of adding a second: got {entry}"
    );
}

/// Issue #104: `tauri build` must not produce an installer.
///
/// The NSIS bundle is what registered the second uninstall key. Setup no
/// longer references it, so a bundle coming back would not break anything
/// visibly — it would just quietly reappear in Installed apps the next time
/// somebody installed a build made from a tree where this flag was dropped.
#[test]
fn tauri_build_does_not_bundle() {
    let makefile = workspace_file("Makefile.toml");

    let task = makefile
        .split_once("[tasks.build_tauri]")
        .expect("Makefile.toml should have a build_tauri task")
        .1;
    let command = task
        .lines()
        .find(|l| l.contains("tauri build"))
        .expect("build_tauri should run tauri build");

    assert!(
        command.contains("--no-bundle"),
        "tauri build must be told not to bundle, or the NSIS installer that \
         put a second Azookey in Installed apps comes back: got {command}"
    );

    let conf = workspace_file("frontend/src-tauri/tauri.conf.json");
    let json: serde_json::Value = serde_json::from_str(&conf).expect("tauri.conf.json is JSON");
    assert_eq!(
        json.pointer("/bundle/active").and_then(|v| v.as_bool()),
        Some(false),
        "bundling must also be off in the config, so building the app by hand \
         does not produce one either"
    );
    assert!(
        json.pointer("/bundle/icon").is_some(),
        "the icon list stays: tauri-build embeds the exe's icon from it, and \
         the single Installed apps entry shows that icon"
    );
}

/// Neither `schtasks` call may have its result discarded.
///
/// The startup task failed to be created on every install for days, and
/// nothing said so: `ShellExec`'s return value and the exit code it writes
/// were both thrown away (issue #83). The install can legitimately continue
/// without the task, but it must not do so silently.
#[test]
fn task_creation_failures_are_reported() {
    let iss = read("Installer.iss");
    let start = iss
        .find("procedure UpdateTaskXml")
        .expect("Installer.iss should define UpdateTaskXml");
    let rest = &iss[start..];
    let end = rest[1..]
        .find("\nprocedure ")
        .map(|i| i + 1)
        .unwrap_or(rest.len());
    let proc_body = &rest[..end];

    assert!(
        !proc_body.contains("ShellExec("),
        "UpdateTaskXml must not call ShellExec directly — go through the \
         helper that checks the result"
    );
    assert!(
        proc_body.contains("MsgBox("),
        "a failure to create the startup task must reach the user; without it \
         the only symptom is an IME that never starts at logon"
    );
}

/// launch.vbs is generated by [Code] in Installer.iss. The path handed to
/// WScript.Shell.Run must be quoted inside the VBScript string (doubled
/// quotes), or CreateProcess prefix-guessing lets a planted
/// `C:\Program.exe` run instead — elevated, since the task runs with
/// RunLevel=HighestAvailable.
#[test]
fn launch_vbs_generation_quotes_the_exe_path() {
    let iss = read("Installer.iss");

    let run_line = iss
        .lines()
        .find(|l| l.contains("objShell.Run"))
        .expect("Installer.iss should generate an objShell.Run line");

    assert!(
        run_line.contains(r#"objShell.Run """#) && run_line.contains(r#"""","#),
        "the launcher path must be wrapped in doubled VBScript quotes: \
         got {run_line}"
    );
}

/// Uninstall must stop the running IME processes, or their exe/dll files
/// stay locked and {app} can't be removed.
#[test]
fn uninstall_stops_the_running_processes() {
    let iss = read("Installer.iss");

    assert!(
        iss.contains(r#"Filename: "taskkill""#),
        "uninstall should invoke taskkill"
    );

    // Inno line-continues each field, so the process list is on the
    // Parameters line, separate from the Filename line
    let params = iss
        .lines()
        .find(|l| l.contains("/IM") && l.contains("launcher.exe"))
        .expect("Installer.iss should pass the processes to taskkill via /IM");

    for proc in [
        "launcher.exe",
        "ui.exe",
        "azookey-server.exe",
        "plugin-host.exe",
    ] {
        assert!(
            params.contains(proc),
            "uninstall's taskkill must target {proc}: got {params}"
        );
    }
}

/// Issue #54: before the WebView2 profile moved under %LOCALAPPDATA%,
/// ui.exe created it inside {app}. Setup never installed that folder, so
/// Setup never removes it — {app} survives an uninstall with a browser
/// profile in it. Same reasoning as launch.vbs above.
#[test]
fn uninstall_removes_the_legacy_webview2_profile() {
    let iss = read("Installer.iss");

    // a section ends at the next header, which is a '[' at the START of a
    // line — splitting on a bare '[' would stop at the "[Code]" inside the
    // very first comment
    let section: String = iss
        .split_once("[UninstallDelete]")
        .expect("Installer.iss should have an [UninstallDelete] section")
        .1
        .lines()
        .take_while(|l| !l.starts_with('['))
        .collect::<Vec<_>>()
        .join("\n");

    // an entry line, not the comment above it that names the same folder
    let entry = section
        .lines()
        .find(|l| l.starts_with("Type:") && l.contains("ui.exe.WebView2"))
        .unwrap_or_else(|| {
            panic!("[UninstallDelete] must remove the legacy profile: got {section}")
        });

    assert!(
        entry.contains("filesandordirs"),
        "the profile is a directory tree, so Type: files would leave it \
         behind: got {entry}"
    );
}

/// Returns the body of a `[Code]` procedure/function, from its header to
/// the next top-level declaration.
fn code_block(iss: &str, header: &str) -> String {
    let start = iss
        .find(header)
        .unwrap_or_else(|| panic!("Installer.iss should define {header}"));
    let rest = &iss[start..];
    let end = rest[1..]
        .find("\nprocedure ")
        .into_iter()
        .chain(rest[1..].find("\nfunction "))
        .min()
        .map(|i| i + 1)
        .unwrap_or(rest.len());
    rest[..end].to_string()
}

/// Issue #4: an upgrade installs over a running stack, whose executables
/// are locked, so installation must stop it before the [Files] copies —
/// not only on uninstall.
#[test]
fn install_stops_the_running_stack_before_copying() {
    let iss = read("Installer.iss");

    assert!(
        iss.contains("function PrepareToInstall("),
        "the stack must be stopped from PrepareToInstall, which also runs \
         for a silent install (a wizard-page hook would not)"
    );

    let body = code_block(&iss, "procedure StopRunningStack");
    for proc in [
        "launcher.exe",
        "ui.exe",
        "azookey-server.exe",
        "plugin-host.exe",
        "Azookey.exe",
    ] {
        assert!(
            body.contains(proc),
            "install-time taskkill must target {proc}: got {body}"
        );
    }
    assert!(
        body.contains("/End /TN \"Azookey Startup\""),
        "the startup task must be ended, or it can relaunch the stack \
         between the kill and the copies"
    );
}

/// Issue #100: the TIP is loaded into every process that accepts text input
/// — Explorer, the shell's search host, every editor and browser — and none
/// of those are ours to stop. `DeleteFile` on a mapped image fails with
/// ERROR_ACCESS_DENIED, so an upgrade could not replace `azookey.dll`: Setup
/// offered "retry", which never succeeds, or "skip", which leaves the OLD
/// TIP registered with everything else new. Renaming a mapped image *is*
/// allowed, so the old file is moved aside before the copies instead.
///
/// The test reads the mechanism, not just the call: a helper that only tried
/// harder to delete would satisfy the name and fix nothing.
#[test]
fn install_moves_the_in_use_tip_dll_aside() {
    let iss = read("Installer.iss");

    let prepare = code_block(&iss, "function PrepareToInstall(");
    assert!(
        prepare.contains("MakeWayForTheTip"),
        "the DLLs must be moved aside from PrepareToInstall, which runs \
         before the copies and also for a silent install: got {prepare}"
    );

    let move_aside = code_block(&iss, "function MoveAsideInUseFile");
    assert!(
        move_aside.contains("RenameFile"),
        "the file has to be RENAMED; deleting it is the operation that \
         already fails: got {move_aside}"
    );

    let make_way = code_block(&iss, "procedure MakeWayForTheTip");
    for dll in ["azookey.dll", "azookey32.dll", "vcruntime140.dll"] {
        assert!(
            make_way.contains(dll),
            "{dll} must be moved aside too — Setup is a 32-bit process and can \
             hold the 32-bit TIP itself, and the host maps the TIP's MSVC \
             runtime out of {{app}} with it: got {make_way}"
        );
    }

    // a renamed file that is still loaded cannot be deleted either, so the
    // leftovers are somebody's job later: the next upgrade, and the uninstall
    let sweep = code_block(&iss, "procedure SweepMovedAsideFiles");
    assert!(
        sweep.contains("FindFirst") && sweep.contains("DeleteFile"),
        "the leftovers must be swept by a later run: got {sweep}"
    );
    assert!(
        make_way.contains("SweepMovedAsideFiles"),
        "the sweep must run on every upgrade, or the moved-aside copies \
         accumulate one per install: got {make_way}"
    );

    let uninstall_delete: String = iss
        .split_once("[UninstallDelete]")
        .expect("Installer.iss should have an [UninstallDelete] section")
        .1
        .lines()
        .take_while(|l| !l.starts_with('['))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        uninstall_delete
            .lines()
            .any(|l| l.starts_with("Type:") && l.contains(".dll.old-*")),
        "the last install has no next upgrade to sweep after it: got \
         {uninstall_delete}"
    );
}

/// The move-aside list has to cover what the TIP *pulls in*, not just the TIP.
///
/// msctf loads an in-proc TIP with the DLL's own directory on the search path,
/// so a host application maps `{app}\vcruntime140.dll` along with the TIP —
/// the same lock `post_build` documents for `build/`. Moving only the two TIP
/// DLLs aside got the copies past `azookey.dll` and then failed on
/// `vcruntime140.dll` with the identical DeleteFile error 5 (measured in the
/// VM, 2026-07-30).
///
/// Derived from the DLL's own bytes rather than restated here: the names in a
/// PE import table are plain ASCII, so every `*.dll` the TIP names and that we
/// ship beside it must appear in `MakeWayForTheTip`. A stray match from an
/// unrelated string only makes this stricter, never laxer.
#[test]
fn the_move_aside_list_covers_what_the_tip_loads_beside_itself() {
    let build = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build");
    let tip = build.join("azookey_windows.dll");
    let Ok(bytes) = std::fs::read(&tip) else {
        // a fresh checkout has no build/; CI runs the tests after the build,
        // and this is the only test here that needs a built artifact
        eprintln!("skipped: {} has not been built", tip.display());
        return;
    };

    let shipped: Vec<String> = std::fs::read_dir(&build)
        .expect("build/ exists, its DLL was just read")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().to_lowercase())
        .filter(|name| name.ends_with(".dll"))
        .collect();

    let make_way = code_block(&read("Installer.iss"), "procedure MakeWayForTheTip").to_lowercase();

    let mut checked = 0;
    for name in dll_names_in(&bytes) {
        // the TIP itself is installed under another name and handled by it
        if !shipped.contains(&name) || name == "azookey_windows.dll" {
            continue;
        }
        checked += 1;
        assert!(
            make_way.contains(&name),
            "the TIP imports {name} and we ship it next to the TIP, so a host \
             application maps it out of the install directory too — it has to \
             be moved aside with the TIP, or the upgrade dies on it: got \
             {make_way}"
        );
    }
    assert!(
        checked > 0,
        "the TIP is expected to name at least one DLL we ship beside it \
         (vcruntime140.dll); finding none means this test stopped looking"
    );
}

/// The `*.dll` names appearing as ASCII in a PE image — its import table
/// stores them as plain, NUL-terminated strings.
fn dll_names_in(bytes: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    let mut current = String::new();
    for &byte in bytes {
        let c = byte as char;
        if byte.is_ascii_alphanumeric() || "_-.+".contains(c) {
            current.push(c.to_ascii_lowercase());
        } else {
            if current.ends_with(".dll") && current.len() > 4 {
                names.push(std::mem::take(&mut current));
            } else {
                current.clear();
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

/// Issue #57: taskkill can only match an image NAME, and `ui.exe` and
/// `launcher.exe` are generic enough to belong to something else entirely,
/// so `/IM` reached every one of them on the machine. Ownership is decided
/// by the executable's PATH instead.
#[test]
fn install_kills_only_processes_from_the_install_directory() {
    let iss = read("Installer.iss");

    let body = code_block(&iss, "function StackProcesses");

    assert!(
        body.contains("ExecutablePath"),
        "ownership must be decided by the executable path, which only WMI \
         exposes: got {body}"
    );
    assert!(
        body.contains("Pos(Prefix, Path) = 1"),
        "the path must be matched against the install directory as a \
         prefix: got {body}"
    );
    // the two unambiguous names stay name-matched on purpose — and for the
    // settings app it also covers an upgrade from a build whose chained
    // installer put it somewhere other than {app}
    for name in ["azookey-server.exe", "azookey.exe"] {
        assert!(
            body.contains(name),
            "{name} is unambiguous and must still be matched by name: got {body}"
        );
    }
}

/// The WMI query is what actually decides which processes are found, and
/// nothing was checking it. `install_stops_the_running_stack_before_copying`
/// reads `StopRunningStack`, where the executable names appear only in the
/// no-WMI fallback — so deleting a name from the query left every test
/// green while an upgrade quietly stopped stopping that process and hit a
/// locked file instead.
#[test]
fn the_process_query_names_every_process_the_installer_must_stop() {
    let iss = read("Installer.iss");

    let body = code_block(&iss, "function StackProcesses");
    let query = body
        .split("ExecQuery")
        .nth(1)
        .expect("StackProcesses should run a WMI query");

    for name in [
        "launcher.exe",
        "ui.exe",
        "azookey-server.exe",
        "plugin-host.exe",
        "Azookey.exe",
    ] {
        assert!(
            query.contains(name),
            "the WMI query must ask for {name}, or an upgrade leaves it \
             running and holding its own file: got {query}"
        );
    }
}

/// Issue #57: terminating a process only REQUESTS it; the handles close a
/// moment later. A fixed sleep proved nothing on a slow machine and wasted
/// the wait on a fast one, so the exit has to be observed.
#[test]
fn install_waits_for_the_stack_to_actually_exit() {
    let iss = read("Installer.iss");

    let body = code_block(&iss, "procedure StopRunningStack");

    assert!(
        body.contains("repeat") && body.contains("until"),
        "the teardown must poll for the processes to be gone: got {body}"
    );
    assert!(
        body.contains("Alive = 0"),
        "the poll must exit on the process count reaching zero: got {body}"
    );
}

/// launcher.exe runs as administrator, and the startup task is
/// machine-scope. `runascurrentuser` de-elevates the spawned process, so a
/// taskkill carrying it silently fails to kill the launcher and the task
/// deletion is denied — the exact lock the kill exists to prevent.
#[test]
fn process_teardown_is_not_de_elevated() {
    let iss = read("Installer.iss");

    let start = iss
        .find("[UninstallRun]")
        .expect("Installer.iss should have an [UninstallRun] section");
    let rest = &iss[start..];
    let end = rest[1..].find("\n[").map(|i| i + 1).unwrap_or(rest.len());
    // directives only: an Inno comment line starts with ';', and the comment
    // above these entries names the flag in order to explain its absence
    let directives: String = rest[..end]
        .lines()
        .filter(|l| !l.trim_start().starts_with(';'))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        !directives.contains("runascurrentuser"),
        "uninstall's taskkill/schtasks must stay elevated: got {directives}"
    );
}

/// The build-directory glob must not re-copy the TIP DLL: it is already
/// placed and registered as azookey.dll / azookey32.dll, so the glob would
/// only add unregistered dead copies.
///
/// Issue #53: the exclusion has to be a wildcard, not the bare filename. A
/// working tree accumulates versioned deploy copies — one per regsvr32
/// target — and an exact-name exclusion shipped every one of them: ~100 MB
/// of dead weight, and a stale DLL next to the live one for a future
/// regsvr32 to pick by mistake.
#[test]
fn build_glob_excludes_every_tip_dll_copy() {
    let iss = read("Installer.iss");

    let glob_line = iss
        .lines()
        .find(|l| l.contains(r#"Source: "../build/*""#))
        .expect("Installer.iss should have a build/* glob");

    let excludes = glob_line
        .split_once("Excludes: \"")
        .and_then(|(_, rest)| rest.split_once('"'))
        .map(|(list, _)| list)
        .expect("the build/* glob should carry an Excludes list");

    let pattern = excludes
        .split(',')
        .find(|p| p.contains("azookey_windows"))
        .unwrap_or_else(|| panic!("the glob must exclude the TIP DLL: got {excludes}"));

    assert!(
        pattern.contains('*'),
        "the exclusion must be a wildcard, or the versioned deploy copies \
         (azookey_windows_batch10.dll and friends) ship too: got {pattern}"
    );

    // the pattern must still cover the plain name the [Files] entries above
    // place as azookey.dll / azookey32.dll
    let prefix = pattern.split('*').next().unwrap_or_default();
    assert!(
        "azookey_windows.dll".starts_with(prefix),
        "the exclusion must still match azookey_windows.dll itself: got {pattern}"
    );

    // hand-made backups of a binary before swapping it are the same kind of
    // working-tree leftover, and two of them were shipping
    assert!(
        excludes.split(',').any(|p| p.trim() == "*.bak"),
        "the build/* glob must exclude *.bak: got {excludes}"
    );
}

/// The product version is single-sourced from [workspace.package] in the
/// root Cargo.toml. The installer must take its version from the
/// generated Version.iss (written by the build_installer task), and the
/// Tauri config must not pin its own copy — omitting "version" makes
/// Tauri fall back to src-tauri's (workspace-inherited) Cargo version.
#[test]
fn product_version_is_single_sourced() {
    let iss = read("Installer.iss");
    assert!(
        iss.contains("#include \"Version.iss\""),
        "Installer.iss must include the generated Version.iss"
    );
    assert!(
        !iss.contains("#define MyAppVersion \""),
        "MyAppVersion must not be hardcoded in Installer.iss"
    );

    let tauri_conf_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../frontend/src-tauri/tauri.conf.json");
    let tauri_conf = std::fs::read_to_string(&tauri_conf_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", tauri_conf_path.display()));
    assert!(
        !tauri_conf.contains("\"version\""),
        "tauri.conf.json must not pin its own version; it inherits the \
         workspace version through src-tauri/Cargo.toml"
    );
}
