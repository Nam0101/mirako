//! The Gradle init script that makes Android Studio / `./gradlew` builds go through `mirako`.

use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;

pub const INIT_SCRIPT: &str = r#"// ~/.gradle/init.d/mirako.gradle — installed by `mirako gradle-shim install`.
// Sends every Gradle build (terminal and Android Studio) through the `mirako` binary.
//   one build locally:        ./gradlew <task> -x mirako      (or -Pmirako.disabled)
//   one project always local: mirako.enabled=false in its local.properties
//   back to local builds:     delete this file
import java.security.MessageDigest

def sp = gradle.startParameter
if (sp.taskNames.isEmpty() || sp.dryRun) return
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

// fast handshake; when the host is down the build simply stays local and untouched
def check = [bin, "check", "--project", root.path].execute()
check.waitFor()
if (check.exitValue() != 0) { println("mirako: ${check.err.text.trim()} — building locally"); return }

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
    }
}
"#;

pub fn init_script(bin: &str) -> String {
    INIT_SCRIPT.replace("__BIN__", bin)
}

pub fn install() -> Result<PathBuf> {
    let bin = std::env::current_exe()?.canonicalize()?;
    let dir = dirs::home_dir().context("no home dir")?.join(".gradle").join("init.d");
    fs::create_dir_all(&dir)?;
    let path = dir.join("mirako.gradle");
    fs::write(&path, init_script(&bin.to_string_lossy()))?;
    Ok(path)
}
