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
// tasks that talk to a device attached to this machine (adb), which the host has not. `install<Variant>` of a debug or
// release variant becomes `assemble<Variant>` there and an `adb install` of the pulled APK here (adbInstall below);
// any other one keeps the whole build local
def installs = sp.taskNames.findAll { it ==~ /(.*:)?install([A-Z]\w*)?(Debug|Release)(AndroidTest)?/ }
def deviceTask = sp.taskNames.find { !(it in installs) && it ==~ /(.*:)?((install|uninstall|connected)[A-Z]\w*|deviceCheck)/ }
if (deviceTask != null) { println("mirako: $deviceTask may need a device attached to this machine, building locally"); return }
// a test run of the IDE (IntelliJ, Android Studio) shows in the init scripts it passes: `ijTestLogger…` for its test console,
// and `ijTestInit…` when the tests go as tasks (`:app:testDebugUnitTest --tests …`). Without the second one it asked Gradle's
// test launcher for them (its default from Gradle 8.3 on), which looks the test tasks up in the build it runs: that build
// stays local. So does a debug run, whose debugger waits for a JVM of this machine.
def ideTests = sp.initScripts.any { it.name.startsWith("ijTestLogger") }
if (ideTests && !sp.initScripts.any { it.name.startsWith("ijTestInit") }) { println("mirako: the IDE asks Gradle's test launcher for these tests, and that needs the project itself: building locally (with the IDE registry key gradle.testLauncherAPI.enabled off they run on the host)"); return }
if (sp.systemPropertiesArgs.containsKey("idea.debugger.dispatch.addr")) { println("mirako: the debugger of the IDE attaches to a JVM on this machine, building locally"); return }

def bin = System.getenv("MIRAKO_BIN") ?: "__BIN__"
if (!new File(bin).canExecute()) { println("mirako: binary not found at $bin, building locally"); return }

__CHECK__

// reconstruct the invocation for the remote ./gradlew
def args = []
args += sp.taskNames.collect { it in installs ? it.replaceFirst(/(^|:)install(?=[A-Z]\w*$)/, '$1assemble') : it }
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

def run = { List<String> cmd ->
    def proc = new ProcessBuilder(cmd).redirectErrorStream(true).start()
    def out = proc.inputStream.text
    [proc.waitFor(), out]
}
def adbPath = {
    def sdk = new Properties()
    if (localProps.exists()) localProps.withInputStream { sdk.load(it) }
    def home = sdk.getProperty("sdk.dir") ?: System.getenv("ANDROID_HOME") ?: System.getenv("ANDROID_SDK_ROOT")
    home ? new File(home, "platform-tools/adb").path : "adb"
}
// the devices an install goes to: `ANDROID_SERIAL`, or every one attached. None fails the build, and before the remote
// one starts (doFirst of the task below) rather than after it
def attached = { String adb ->
    def listed = run([adb, "devices"])[1]
    def devices = System.getenv("ANDROID_SERIAL") ? [System.getenv("ANDROID_SERIAL")] : listed.readLines().findAll { it.endsWith("\tdevice") }.collect { it.split("\t")[0] }
    if (devices.isEmpty()) throw new GradleException("mirako: no device to install on:\n$listed")
    devices
}

// `adb install`, on every device attached, of the APK that `assemble<Variant>` left under build/outputs/apk and the pull brought here
def adbInstall = { String task ->
    def name = task.tokenize(":").last().substring("install".length())
    def variant = name[0].toLowerCase() + name.substring(1)
    def base = new File(root, task.tokenize(":").dropRight(1).join("/"))
    if (!base.directory) base = root
    def apks = []
    def skip = { File d -> d.name.startsWith(".") || d.name in ["src", "node_modules"] || (d.parentFile.name == "build" && d.name != "outputs") }
    base.traverse(type: groovy.io.FileType.FILES, nameFilter: "output-metadata.json",
            preDir: { skip(it) ? groovy.io.FileVisitResult.SKIP_SUBTREE : groovy.io.FileVisitResult.CONTINUE }) { meta ->
        if (!meta.path.replace("\\", "/").contains("/build/outputs/apk/")) return
        def json = new groovy.json.JsonSlurper().parse(meta)
        if (json.artifactType?.type != "APK" || json.variantName != variant) return
        if (json.elements.size() != 1) throw new GradleException("mirako: $variant has ${json.elements.size()} APKs (splits) and which one a device takes is AGP's call: run this build with -x mirako")
        apks << new File(meta.parentFile, json.elements[0].outputFile)
    }
    if (apks.isEmpty()) throw new GradleException("mirako: no APK of variant $variant under $base (build/outputs/apk/**/output-metadata.json)")
    def adb = adbPath()
    attached(adb).each { device ->
        apks.each { apk ->
            def (code, out) = run([adb, "-s", device, "install", "-r", "-t", apk.path])
            if (code != 0) throw new GradleException("mirako: adb install of ${apk.name} on $device failed:\n$out")
            println("mirako: installed ${apk.name} on $device")
        }
    }
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
        // `--test-events`: the tests run where the IDE cannot listen to them, so the build prints them the way its console reads them
        t.commandLine([bin, "run", "--project", projectRoot.path] + (ideTests ? ["--test-events"] : []) + ["--", "./gradlew"] + gradleArgs)
        t.doNotTrackState("mirako is never up-to-date")
        t.notCompatibleWithConfigurationCache("a reused entry would replay the flags of an earlier invocation")
        t.doFirst { if (installs) attached(adbPath()) }
        t.doLast { installs.each { adbInstall(it) } }
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

/// Where `mirako run --test-events` keeps `TEST_EVENTS_SCRIPT`, relative to the project root: in
/// `.gradle`, which no VCS tracks, and uploaded all the same (see `test_events`).
pub const TEST_EVENTS: &str = ".gradle/mirako-test-events.gradle";

/// The init script of a build whose tests the IDE wants to see. Public Gradle API only (run on Gradle 8.13 and 9.8).
pub const TEST_EVENTS_SCRIPT: &str = r#"// .gradle/mirako-test-events.gradle — written by `mirako run --test-events`, which the Gradle shim passes for a test run of the IDE.
// The tests of this build run where the IDE cannot listen to them: every test task reports them on stdout instead, one
// `<ijLog>` line per event, the form the Gradle test console of IntelliJ and Android Studio reads out of the build output.
import org.gradle.api.tasks.testing.AbstractTestTask
import org.gradle.api.tasks.testing.TestDescriptor
import org.gradle.api.tasks.testing.TestListener
import org.gradle.api.tasks.testing.TestOutputListener
import org.gradle.api.tasks.testing.TestResult
import java.util.concurrent.atomic.AtomicLong

def ids = Collections.synchronizedMap(new IdentityHashMap())
def lastId = new AtomicLong()
def idOf = { TestDescriptor d -> d == null ? "" : ids.computeIfAbsent(d) { lastId.incrementAndGet().toString() } }
// an event is one line of XML: nothing below a space in an attribute, text as base64
def attr = { v -> (v ?: "").toString().replaceAll(/[\x00-\x1f]/, " ").replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;").replace("'", "&apos;") }
def text = { v -> "<![CDATA[" + Base64.encoder.encodeToString((v ?: "").toString().getBytes("UTF-8")) + "]]>" }
def event = { String type, TestDescriptor d, String body ->
    println("<ijLog><event type='$type'><test id='${idOf(d)}' parentId='${idOf(d.parent)}'>" +
            "<descriptor name='${attr(d.name)}' displayName='${attr(d.displayName)}' className='${attr(d.className)}'/>$body</test></event></ijLog>")
}
def result = { TestResult r ->
    def body = ""
    if (r.resultType == TestResult.ResultType.FAILURE) {
        def f = r.failures ? r.failures[0].details : null
        def type = f == null ? "error" : f.expected != null || f.actual != null ? "comparison" : f.assertionFailure ? "assertionFailed" : "error"
        body = "<errorMsg>${text(f?.message)}</errorMsg><exceptionName>${text(f?.className)}</exceptionName><stackTrace>${text(f?.stacktrace)}</stackTrace><failureType>$type</failureType>"
        if (type == "comparison") body += "<expected>${text(f.expected)}</expected><actual>${text(f.actual)}</actual>"
    }
    "<result resultType='${r.resultType}' startTime='${r.startTime}' endTime='${r.endTime}'>$body</result>"
}

gradle.taskGraph.whenReady { graph ->
    graph.allTasks.findAll { it instanceof AbstractTestTask }.each { task ->
        task.outputs.upToDateWhen { false }   // the IDE asked for a run, not for the results of the last one
        task.testLogging.showStandardStreams = false
        println("<ijLog><event type='reportLocation' testReport='${attr(task.reports.html.entryPoint.path)}'/></ijLog>")
        task.addTestListener([
            beforeSuite: { d -> event("beforeSuite", d, "") },
            afterSuite : { d, r -> event("afterSuite", d, result(r)) },
            beforeTest : { d -> event("beforeTest", d, "") },
            afterTest  : { d, r -> event("afterTest", d, result(r)) },
        ] as TestListener)
        task.addTestOutputListener({ d, e -> event("onOutput", d, "<event destination='${e.destination}'>${text(e.message)}</event>") } as TestOutputListener)
    }
}
"#;

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

/// `mirako run --test-events`: puts `TEST_EVENTS_SCRIPT` into the project, from where the push takes
/// it to the host (and a local fallback finds it), and returns the Gradle arguments that load it.
/// Without the configuration cache, an entry of which runs no init script and so reports no test.
pub fn test_events(root: &Path) -> Result<Vec<String>> {
    let path = root.join(TEST_EVENTS);
    // written once: an unchanged file is neither hashed nor sent again
    if fs::read_to_string(&path).ok().as_deref() != Some(TEST_EVENTS_SCRIPT) {
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(&path, TEST_EVENTS_SCRIPT).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(["--init-script", TEST_EVENTS, "--no-configuration-cache"]
        .map(String::from)
        .to_vec())
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
            "!(it in installs) && it ==~ /(.*:)?((install|uninstall|connected)[A-Z]\\w*|deviceCheck)/",
            "ideTests && !sp.initScripts.any { it.name.startsWith(\"ijTestInit\") }",
            "containsKey(\"idea.debugger.dispatch.addr\")",
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
        assert!(INIT_SCRIPT.contains(
            "[bin, \"run\", \"--project\", projectRoot.path] + (ideTests ? [\"--test-events\"] : []) + [\"--\", \"./gradlew\"] + gradleArgs"
        ));
    }

    #[test]
    fn init_script_asks_for_test_events_on_a_test_run_of_the_ide() {
        // the names IntelliJ gives its init scripts: `ijTestLogger<n>.gradle`, `ijTestInit<n>.gradle`
        assert!(INIT_SCRIPT.contains("def ideTests = sp.initScripts.any { it.name.startsWith(\"ijTestLogger\") }"));
        // what its test console takes for an event: a line of the build output from `<ijLog>` to `</ijLog>`
        assert_eq!(TEST_EVENTS_SCRIPT.matches("println(\"<ijLog><event type='").count(), 2);
        assert_eq!(TEST_EVENTS_SCRIPT.matches("</ijLog>\")").count(), 2);
        for kind in [
            "'reportLocation'",
            "\"beforeSuite\"",
            "\"afterSuite\"",
            "\"beforeTest\"",
            "\"afterTest\"",
            "\"onOutput\"",
        ] {
            assert!(TEST_EVENTS_SCRIPT.contains(kind), "missing {kind}");
        }
    }

    #[test]
    fn test_events_puts_the_script_into_the_project_and_returns_the_gradle_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let args = test_events(dir.path()).unwrap();
        assert_eq!(
            args,
            ["--init-script", ".gradle/mirako-test-events.gradle", "--no-configuration-cache"]
        );
        let path = dir.path().join(TEST_EVENTS);
        assert_eq!(fs::read_to_string(&path).unwrap(), TEST_EVENTS_SCRIPT);
        // the script of another version is replaced
        fs::write(&path, "// old").unwrap();
        test_events(dir.path()).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), TEST_EVENTS_SCRIPT);
    }

    #[test]
    fn init_script_builds_an_install_task_remotely_and_installs_the_apk_here() {
        // only a debug or release variant: `installDist`, `installGitHooks` are no Android installs
        assert!(INIT_SCRIPT.contains("it ==~ /(.*:)?install([A-Z]\\w*)?(Debug|Release)(AndroidTest)?/"));
        assert!(INIT_SCRIPT.contains("it.replaceFirst(/(^|:)install(?=[A-Z]\\w*$)/, '$1assemble')"));
        // no device: known before the build, not after it
        assert!(INIT_SCRIPT.contains("t.doFirst { if (installs) attached(adbPath()) }"));
        assert!(INIT_SCRIPT.contains("t.doLast { installs.each { adbInstall(it) } }"));
        assert!(INIT_SCRIPT.contains("[adb, \"-s\", device, \"install\", \"-r\", \"-t\", apk.path]"));
    }
}
