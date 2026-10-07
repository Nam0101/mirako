//! The Gradle init script that makes Android Studio / `./gradlew` builds go through `mirako`.

use crate::config;
use crate::xfer::canonical;
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

pub const INIT_SCRIPT: &str = r#"// ~/.gradle/init.d/mirako.gradle — installed by `mirako gradle-shim install`, kept current by `mirako run`.
// Sends every Gradle build (terminal and Android Studio) through the `mirako` binary.
//   one build locally:        ./gradlew <task> -x mirako      (or -Pmirako.disabled)
//   one project always local: mirako.enabled=false in its local.properties
//   skip the handshake:       shim_check = false in ~/.config/mirako/config.toml
//   back to local builds:     delete this file
import java.security.MessageDigest

def sp = gradle.startParameter
if (sp.taskNames.isEmpty() || sp.dryRun) return
if (sp.taskNames.any { it in ["updateDaemonJvm", ":updateDaemonJvm", "wrapper", ":wrapper"] }) return   // edits the project's gradle config: stays local
if (sp.projectProperties.containsKey("mirako.disabled")) return
if (sp.excludedTaskNames.remove("mirako")) return
if (System.getenv("MIRAKO_REMOTE") == "1") return   // this is already the remote build
if (System.getenv("MIRAKO_LOCAL") == "1") return    // mirako already fell back to a local build

def root = sp.currentDir
while (root != null && !new File(root, "gradlew").exists()) root = root.parentFile
if (root == null) return
def localProps = new File(root, "local.properties")
if (localProps.exists() && localProps.text.contains("mirako.enabled=false")) return

def bin = System.getenv("MIRAKO_BIN") ?: "__BIN__"
if (!new File(bin).canExecute()) { println("mirako: binary not found at $bin, building locally"); return }

__CHECK__

// reconstruct the invocation for the remote ./gradlew
def args = []
args += sp.taskNames
sp.excludedTaskNames.each { args += ["-x", it] }
sp.projectProperties.findAll { k, v -> k != "android.injected.attribution.file.location" }.each { k, v -> args += ["-P${k}=${v}".toString()] }
sp.systemPropertiesArgs.each { k, v -> args += ["-D${k}=${v}".toString()] }
if (sp.offline) args += "--offline"
if (sp.refreshDependencies) args += "--refresh-dependencies"
if (sp.rerunTasks) args += "--rerun-tasks"
if (sp.continueOnFailure) args += "--continue"
if (sp.parallelProjectExecutionEnabled) args += "--parallel"
if (sp.configureOnDemand) args += "--configure-on-demand"
if (sp.buildScan) args += "--scan"
switch (sp.logLevel.toString()) {
    case "DEBUG": args += "--debug"; break
    case "INFO":  args += "--info"; break
    case "WARN":  args += "--warn"; break
    case "QUIET": args += "--quiet"; break
}
switch (sp.showStacktrace.toString()) {
    case "ALWAYS":      args += "--stacktrace"; break
    case "ALWAYS_FULL": args += "--full-stacktrace"; break
}
switch (sp.consoleOutput.toString()) {
    case "Plain": args += ["--console", "plain"]; break
    case "Rich":  args += ["--console", "rich"]; break
}

// point Gradle at an empty stub project so the real build scripts are neither evaluated nor touched here
def digest = MessageDigest.getInstance("SHA-1").digest(root.path.bytes).encodeHex().toString().substring(0, 12)
def stub = new File(sp.gradleUserHomeDir, "mirako/stubs/$digest")
stub.mkdirs()
new File(stub, "settings.gradle").text = "rootProject.name = '${root.name}'\n"
sp.projectDir = stub
sp.setTaskNames(["mirako"])
sp.setExcludedTaskNames([])

def gradleArgs = args
def projectRoot = root
gradle.rootProject { p ->
    p.tasks.register("mirako", Exec) { t ->
        t.workingDir = projectRoot
        t.commandLine([bin, "run", "--project", projectRoot.path, "--", "./gradlew"] + gradleArgs)
        t.doNotTrackState("mirako is never up-to-date")
        t.notCompatibleWithConfigurationCache("a reused entry would replay the flags of an earlier invocation")
    }
}
"#;

/// The `__CHECK__` block with `shim_check = true` (the default).
pub const CHECK: &str = r#"// fast handshake; when the host is down the build simply stays local and untouched
// (through ProviderFactory: the configuration cache rejects a plain execute() here, and re-runs this one before reusing an entry)
def check = gradle.services.get(org.gradle.api.provider.ProviderFactory).exec {
    it.commandLine(bin, "check", "--project", root.path)
    it.ignoreExitValue = true
}
if (check.result.get().exitValue != 0) { println("mirako: ${check.standardError.asText.get().trim()} — building locally"); return }"#;

const NO_CHECK: &str = "// no handshake (shim_check = false): a dead host fails, or falls back, inside `mirako run`";

pub fn init_script(bin: &str, check: bool) -> String {
    INIT_SCRIPT
        .replace("__BIN__", bin)
        .replace("__CHECK__", if check { CHECK } else { NO_CHECK })
}

/// `~/.gradle/init.d/mirako.gradle`.
fn script_path() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .context("no home dir")?
        .join(".gradle")
        .join("init.d")
        .join("mirako.gradle"))
}

/// This executable's canonical path, the one written into the script. With `/` on Windows too:
/// a `\` is an escape inside the script's Groovy string.
pub fn this_binary() -> Result<String> {
    let exe = canonical(&std::env::current_exe()?)?.to_string_lossy().into_owned();
    Ok(if cfg!(windows) { exe.replace('\\', "/") } else { exe })
}

pub fn install() -> Result<PathBuf> {
    let path = script_path()?;
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(&path, init_script(&this_binary()?, config::shim_check()?))?;
    Ok(path)
}

/// Brings the installed script up to date with this binary and `shim_check`; `Some(path)` when it
/// was rewritten. Nothing happens when there is no script or it names another binary (a dev
/// build must not hijack the installed one).
pub fn refresh() -> Result<Option<PathBuf>> {
    let path = script_path()?;
    Ok(refresh_file(&path, &this_binary()?, config::shim_check()?)?.then_some(path))
}

fn refresh_file(path: &Path, bin: &str, check: bool) -> Result<bool> {
    let Ok(installed) = fs::read_to_string(path) else {
        return Ok(false);
    };
    if !installed.contains(&format!("?: \"{bin}\"")) {
        return Ok(false);
    }
    let want = init_script(bin, check);
    if installed == want {
        return Ok(false);
    }
    fs::write(path, want).with_context(|| format!("rewriting {}", path.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_script_substitutes_the_binary_path_and_the_check_block() {
        let s = init_script("/x/bin/mirako", true);
        assert!(s.contains(r#"System.getenv("MIRAKO_BIN") ?: "/x/bin/mirako""#));
        assert!(!s.contains("__BIN__") && !s.contains("__CHECK__"));
        assert_eq!(INIT_SCRIPT.matches("__BIN__").count(), 1);
        assert_eq!(INIT_SCRIPT.matches("__CHECK__").count(), 1);
        assert!(s.contains(CHECK));
    }

    #[test]
    fn shim_check_false_drops_only_the_handshake() {
        let with = init_script("/x", true);
        let without = init_script("/x", false);
        assert!(!without.contains("\"check\"") && !without.contains("ProviderFactory"));
        assert!(without.contains("shim_check = false"));
        for kept in ["canExecute()", "p.tasks.register(\"mirako\", Exec)", "MIRAKO_LOCAL"] {
            assert!(without.contains(kept), "missing {kept}");
        }
        assert_eq!(with.replace(CHECK, NO_CHECK), without);
    }

    #[test]
    fn refresh_rewrites_a_stale_script_of_this_binary_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mirako.gradle");
        // no script installed: nothing to do
        assert!(!refresh_file(&path, "/me", true).unwrap());
        assert!(!path.exists());
        // up to date: untouched
        fs::write(&path, init_script("/me", true)).unwrap();
        assert!(!refresh_file(&path, "/me", true).unwrap());
        // stale (older text, or shim_check flipped): rewritten
        fs::write(&path, init_script("/me", true).replace("def args = []", "def args = [] // old")).unwrap();
        assert!(refresh_file(&path, "/me", true).unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), init_script("/me", true));
        assert!(refresh_file(&path, "/me", false).unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), init_script("/me", false));
        // another binary's install: left alone
        fs::write(&path, init_script("/other/mirako", true).replace("def args = []", "// old")).unwrap();
        assert!(!refresh_file(&path, "/me", true).unwrap());
        assert!(fs::read_to_string(&path).unwrap().contains("// old"));
    }

    #[test]
    fn init_script_keeps_every_bail_out() {
        let s = init_script("/x", true);
        for marker in [
            "\"updateDaemonJvm\"",
            "\":updateDaemonJvm\"",
            "\"wrapper\"",
            "\":wrapper\"",
            "System.getenv(\"MIRAKO_REMOTE\") == \"1\"",
            "System.getenv(\"MIRAKO_LOCAL\") == \"1\"",
            "containsKey(\"mirako.disabled\")",
            "excludedTaskNames.remove(\"mirako\")",
            "contains(\"mirako.enabled=false\")",
            "it.commandLine(bin, \"check\", \"--project\", root.path)",
            "check.result.get().exitValue != 0",
            "sp.dryRun",
        ] {
            assert!(s.contains(marker), "missing {marker}");
        }
    }

    #[test]
    fn init_script_registers_one_mirako_exec_task_running_gradlew() {
        assert!(INIT_SCRIPT.contains("p.tasks.register(\"mirako\", Exec)"));
        assert!(INIT_SCRIPT.contains("sp.setTaskNames([\"mirako\"])"));
        assert!(INIT_SCRIPT.contains("[bin, \"run\", \"--project\", projectRoot.path, \"--\", \"./gradlew\"]"));
    }
}
