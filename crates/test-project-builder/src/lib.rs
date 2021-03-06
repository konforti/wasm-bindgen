#[macro_use]
extern crate lazy_static;
extern crate wasm_bindgen_cli_support as cli;

use std::env;
use std::fs::{self, File};
use std::thread;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio, Child, ChildStdin};
use std::net::TcpStream;
use std::sync::atomic::*;
use std::sync::{Once, ONCE_INIT, Mutex};
use std::time::{Duration, Instant};

static CNT: AtomicUsize = ATOMIC_USIZE_INIT;
thread_local!(static IDX: usize = CNT.fetch_add(1, Ordering::SeqCst));

pub struct Project {
    files: Vec<(String, String)>,
    debug: bool,
    node: bool,
    nodejs_experimental_modules: bool,
    no_std: bool,
    serde: bool,
    rlib: bool,
    webpack: bool,
    node_args: Vec<String>,
    deps: Vec<String>,
    headless: bool,
}

pub fn project() -> Project {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut lockfile = String::new();
    fs::File::open(&dir.join("Cargo.lock"))
        .unwrap()
        .read_to_string(&mut lockfile)
        .unwrap();
    Project {
        debug: true,
        no_std: false,
        node: true,
        nodejs_experimental_modules: true,
        webpack: false,
        serde: false,
        rlib: false,
        headless: false,
        deps: Vec::new(),
        node_args: Vec::new(),
        files: vec![
            ("Cargo.lock".to_string(), lockfile),
        ],
    }
}

fn root() -> PathBuf {
    let idx = IDX.with(|x| *x);

    let mut me = env::current_exe().unwrap();
    me.pop(); // chop off exe name
    me.pop(); // chop off `deps`
    me.pop(); // chop off `debug` / `release`
    me.push("generated-tests");
    me.push(&format!("test{}", idx));
    return me;
}

fn assert_bigint_support() -> Option<&'static str> {
    static BIGINT_SUPPORED: AtomicUsize = ATOMIC_USIZE_INIT;
    static INIT: Once = ONCE_INIT;

    INIT.call_once(|| {
        let mut cmd = Command::new("node");
        cmd.arg("-e").arg("BigInt");
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if cmd.status().unwrap().success() {
            BIGINT_SUPPORED.store(1, Ordering::SeqCst);
            return;
        }

        cmd.arg("--harmony-bigint");
        if cmd.status().unwrap().success() {
            BIGINT_SUPPORED.store(2, Ordering::SeqCst);
            return;
        }
    });

    match BIGINT_SUPPORED.load(Ordering::SeqCst) {
        1 => return None,
        2 => return Some("--harmony-bigint"),
        _ => panic!(
            "the version of node.js that is installed for these tests \
             does not support `BigInt`, you may wish to try installing \
             node 10 to fix this"
        ),
    }
}

impl Project {
    /// Add a new file with the specified contents to this project, the `name`
    /// can have slahes for files in subdirectories.
    pub fn file(&mut self, name: &str, contents: &str) -> &mut Project {
        self.files.push((name.to_string(), contents.to_string()));
        self
    }

    /// Enable debug mode in wasm-bindgen for this test
    pub fn debug(&mut self, debug: bool) -> &mut Project {
        self.debug = debug;
        self
    }

    /// Depend on `wasm-bindgen` without the `std` feature enabled.
    pub fn no_std(&mut self, no_std: bool) -> &mut Project {
        self.no_std = no_std;
        self
    }

    /// Depend on the `serde` feature of `wasm-bindgen`
    pub fn serde(&mut self, serde: bool) -> &mut Project {
        self.serde = serde;
        self
    }

    /// Generate an rlib instead of a cdylib in the generated Cargo project
    pub fn rlib(&mut self, rlib: bool) -> &mut Project {
        self.rlib = rlib;
        self
    }

    /// Depend on a crate from crates.io, like serde.
    pub fn depend(&mut self, dep: &str) -> &mut Project {
        self.deps.push(dep.to_string());
        self
    }

    /// Enables or disables node.js experimental modules output
    pub fn nodejs_experimental_modules(&mut self, node: bool) -> &mut Project {
        self.nodejs_experimental_modules = node;
        self
    }

    /// Enables or disables the usage of webpack for this project
    pub fn webpack(&mut self, webpack: bool) -> &mut Project {
        self.webpack = webpack;
        self
    }

    /// Add a path dependency to the generated project
    pub fn add_local_dependency(&mut self, name: &str, path: &str) -> &mut Project {
        self.deps
            .push(format!("{} = {{ path = '{}' }}", name, path));
        self
    }

    /// Returns the crate name that will be used for the generated crate, this
    /// name changes between test runs and is generated at runtime.
    pub fn crate_name(&self) -> String {
        format!("test{}", IDX.with(|x| *x))
    }

    /// Flag this project as requiring bigint support in Node
    pub fn requires_bigint(&mut self) -> &mut Project {
        if let Some(arg) = assert_bigint_support() {
            self.node_args.push(arg.to_string());
        }
        self
    }

    /// This test requires a headless web browser
    pub fn headless(&mut self, headless: bool) -> &mut Project {
        self.headless = headless;
        self
    }

    /// Write this project to the filesystem, ensuring all files are ready to
    /// go.
    pub fn build(&mut self) -> (PathBuf, PathBuf) {
        if self.headless {
            self.webpack = true;
        }
        if self.webpack {
            self.node = false;
            self.nodejs_experimental_modules = false;
        }

        self.ensure_webpack_config();
        self.ensure_test_entry();

        if self.headless {
            self.ensure_index_html();
            self.ensure_run_headless_js();
        }

        let webidl_modules = self.generate_webidl_bindings();
        self.generate_js_entry(webidl_modules);

        let mut manifest = format!(
            r#"
            [package]
            name = "test{}"
            version = "0.0.1"
            authors = []

            [workspace]

            [lib]
        "#,
            IDX.with(|x| *x)
        );

        if !self.rlib {
            manifest.push_str("crate-type = [\"cdylib\"]\n");
        }

        manifest.push_str("[build-dependencies]\n");
        manifest.push_str("wasm-bindgen-webidl = { path = '");
        manifest.push_str(env!("CARGO_MANIFEST_DIR"));
        manifest.push_str("/../webidl' }\n");

        manifest.push_str("[dependencies]\n");
        for dep in self.deps.iter() {
            manifest.push_str(dep);
            manifest.push_str("\n");
        }
        manifest.push_str("wasm-bindgen = { path = '");
        manifest.push_str(env!("CARGO_MANIFEST_DIR"));
        manifest.push_str("/../..'");
        if self.no_std {
            manifest.push_str(", default-features = false");
        }
        if self.serde {
            manifest.push_str(", features = ['serde-serialize']");
        }
        manifest.push_str(" }\n");
        self.files.push(("Cargo.toml".to_string(), manifest));

        let root = root();
        drop(fs::remove_dir_all(&root));
        for &(ref file, ref contents) in self.files.iter() {
            let mut dst = root.join(file);
            if self.nodejs_experimental_modules &&
                dst.extension().and_then(|s| s.to_str()) == Some("js")
            {
                dst = dst.with_extension("mjs");
            }
            if dst.extension().and_then(|s| s.to_str()) == Some("ts") &&
                !self.webpack
            {
                panic!("webpack needs to be enabled to use typescript");
            }
            fs::create_dir_all(dst.parent().unwrap()).unwrap();
            fs::File::create(&dst)
                .unwrap()
                .write_all(contents.as_ref())
                .unwrap();
        }
        let target_dir = root.parent().unwrap() // chop off test name
            .parent().unwrap(); // chop off `generated-tests`
        (root.clone(), target_dir.to_path_buf())
    }

    fn ensure_webpack_config(&mut self) {
        if !self.webpack {
            return
        }

        let needs_typescript = self.files.iter().any(|t| t.0.ends_with(".ts"));

        let mut rules = String::new();
        let mut extensions = format!("'.js', '.wasm'");
        if needs_typescript {
            rules.push_str("
                {
                    test: /.ts$/,
                    use: 'ts-loader',
                    exclude: /node_modules/,
                }
            ");
            extensions.push_str(", '.ts'");
        }
        let target = if self.headless { "web" } else { "node" };
        self.files.push((
            "webpack.config.js".to_string(),
            format!(r#"
                const path = require('path');
                const fs = require('fs');

                let nodeModules = {{}};

                // Webpack bundles the modules from node_modules.
                // For node target, we will not have `fs` module
                // inside the `node_modules` folder.
                // This reads the directories in `node_modules`
                // and give that to externals and webpack ignores
                // to bundle the modules listed as external.
                if ('{2}' == 'node') {{
                    fs.readdirSync('node_modules')
                        .filter(module => module !== '.bin')
                        .forEach(mod => {{
                            // External however,expects browser environment.
                            // To make it work in `node` target we
                            // prefix commonjs here.
                            nodeModules[mod] = 'commonjs ' + mod;
                        }});
                }}

                module.exports = {{
                  entry: './run.js',
                  mode: "development",
                  devtool: "source-map",
                  module: {{
                    rules: [{}]
                  }},
                  resolve: {{
                    extensions: [{}]
                  }},
                  output: {{
                    filename: 'bundle.js',
                    path: path.resolve(__dirname, '.')
                  }},
                  target: '{}',
                  externals: nodeModules
                }};
            "#, rules, extensions, target)
        ));
        if needs_typescript {
            self.files.push((
                "tsconfig.json".to_string(),
                r#"
                    {
                      "compilerOptions": {
                        "noEmitOnError": true,
                        "noImplicitAny": true,
                        "noImplicitThis": true,
                        "noUnusedParameters": true,
                        "noUnusedLocals": true,
                        "noImplicitReturns": true,
                        "strictFunctionTypes": true,
                        "strictNullChecks": true,
                        "alwaysStrict": true,
                        "strict": true,
                        "target": "es5",
                        "lib": ["es2015"]
                      }
                    }
                "#.to_string(),
            ));
        }
    }

    fn ensure_test_entry(&mut self) {
        if !self
            .files
            .iter()
            .any(|(path, _)| path == "test.ts" || path == "test.js")
        {
            self.files.push((
                "test.js".to_string(),
                r#"export {test} from './out';"#.to_string(),
            ));
        }
    }

    fn ensure_index_html(&mut self) {
        self.file(
            "index.html",
            r#"
                <!DOCTYPE html>
                <html>
                    <body>
                        <div id="error"></div>
                        <div id="logs"></div>
                        <div id="status"></div>
                        <script src="bundle.js"></script>
                    </body>
                </html>
            "#,
        );
    }

    fn ensure_run_headless_js(&mut self) {
        self.file("run-headless.js", include_str!("run-headless.js"));
    }

    fn generate_webidl_bindings(&mut self) -> Vec<PathBuf> {
        let mut res = Vec::new();
        let mut origpaths = Vec::new();

        for (path, _) in &self.files {
            let path = Path::new(&path);
            let extension = path.extension().map(|x| x.to_str().unwrap());

            if extension != Some("webidl") {
                continue;
            }

            res.push(path.with_extension("rs"));
            origpaths.push(path.to_owned());
        }

        if res.is_empty() {
            return res;
        }

        let mut buildrs = r#"
            extern crate wasm_bindgen_webidl;

            use wasm_bindgen_webidl::compile_file;
            use std::env;
            use std::fs::{self, File};
            use std::io::Write;
            use std::path::Path;

            fn main() {
                let dest = env::var("OUT_DIR").unwrap();
        "#.to_string();

        for (path, origpath) in res.iter().zip(origpaths.iter()) {
            buildrs.push_str(&format!(
                r#"
                fs::create_dir_all("{}").unwrap();
                File::create(&Path::new(&dest).join("{}"))
                    .unwrap()
                    .write_all(
                        compile_file(Path::new("{}"))
                            .unwrap()
                            .as_bytes()
                    )
                    .unwrap();
                "#,
                path.parent().unwrap().to_str().unwrap(),
                path.to_str().unwrap(),
                origpath.to_str().unwrap(),
            ));

            self.files.push((
                Path::new("src").join(path).to_str().unwrap().to_string(),
                format!(
                    r#"include!(concat!(env!("OUT_DIR"), "/{}"));"#,
                    path.display()
                ),
            ));
        }

        buildrs.push('}');

        self.files.push(("build.rs".to_string(), buildrs));

        res
    }

    fn generate_js_entry(&mut self, modules: Vec<PathBuf>) {
        let mut runjs = String::new();
        let esm_imports = self.webpack || !self.node || self.nodejs_experimental_modules;


        if self.headless {
            runjs.push_str(
                r#"
                    window.document.body.innerHTML += "\nTEST_START\n";
                    console.log = function(...args) {
                        const logs = document.getElementById('logs');
                        for (let msg of args) {
                            logs.innerHTML += `${msg}<br/>\n`;
                        }
                    };
                "#,
            );
        } else if esm_imports {
            runjs.push_str("import * as process from 'process';\n");
        } else {
            runjs.push_str("const process = require('process');\n");
        }

        runjs.push_str("
            function run(test, wasm) {
                test.test();

                if (wasm.assertStackEmpty)
                    wasm.assertStackEmpty();
                if (wasm.assertSlabEmpty)
                    wasm.assertSlabEmpty();
            }
        ");

        if self.headless {
            runjs.push_str("
                function onerror(error) {
                    const errors = document.getElementById('error');
                    let content = `exception: ${e.message}\\nstack: ${e.stack}`;
                    errors.innerHTML = `<pre>${content}</pre>`;
                }
            ");
        } else {
            runjs.push_str("
                function onerror(error) {
                    console.error(error);
                    process.exit(1);
                }
            ");
        }

        if esm_imports {
            runjs.push_str("console.log('importing modules...');\n");
            runjs.push_str("const modules = [];\n");
            for module in modules.iter() {
                runjs.push_str(&format!("modules.push(import('./{}'))",
                                        module.with_extension("").display()));
            }
            let import_wasm = if self.debug {
                "import('./out')"
            } else {
                "new Promise((a, b) => a({}))"
            };
            runjs.push_str(&format!("
                Promise.all(modules)
                    .then(results => {{
                        results.map(module => Object.assign(global, module));
                        return Promise.all([import('./test'), {}])
                    }})
                    .then(result => run(result[0], result[1]))
            ", import_wasm));

            if self.headless {
                runjs.push_str(".then(() => {
                    document.getElementById('status').innerHTML = 'good';
                })");
            }
            runjs.push_str(".catch(onerror)\n");

            if self.headless {
                runjs.push_str("
                    .finally(() => {
                        window.document.body.innerHTML += \"\\nTEST_DONE\";
                    })
                ");
            }
        } else {
            assert!(!self.debug);
            assert!(modules.is_empty());
            runjs.push_str("
                const test = require('./test');
                try {
                    run(test, {});
                } catch (e) {
                    onerror(e);
                }
            ");
        }
        self.files.push(("run.js".to_string(), runjs));
    }

    /// Build the Cargo project for the wasm target, returning the root of the
    /// project and the target directory where output is located.
    pub fn cargo_build(&mut self) -> (PathBuf, PathBuf) {
        let (root, target_dir) = self.build();
        let mut cmd = Command::new("cargo");
        cmd.arg("build")
            .arg("--target")
            .arg("wasm32-unknown-unknown")
            .current_dir(&root)
            .env("CARGO_TARGET_DIR", &target_dir)
            // Catch any warnings in generated code because we don't want any
            .env("RUSTFLAGS", "-Dwarnings");
        run(&mut cmd, "cargo");
        (root, target_dir)
    }

    /// Generate wasm-bindgen bindings for the compiled artifacts of this
    /// project, returning the root of the project as well as the target
    /// directory where output was generated.
    pub fn gen_bindings(&mut self) -> (PathBuf, PathBuf) {
        let (root, target_dir) = self.cargo_build();
        let idx = IDX.with(|x| *x);
        let out = target_dir.join(&format!("wasm32-unknown-unknown/debug/test{}.wasm", idx));

        let as_a_module = root.join("out.wasm");
        fs::hard_link(&out, &as_a_module).unwrap();

        let _x = wrap_step("running wasm-bindgen");
        let res = cli::Bindgen::new()
            .input_path(&as_a_module)
            .typescript(self.webpack)
            .debug(self.debug)
            .nodejs(self.node)
            .nodejs_experimental_modules(self.nodejs_experimental_modules)
            .generate(&root);

        if let Err(e) = res {
            for e in e.causes() {
                println!("- {}", e);
            }
            panic!("failed");
        }

        (root, target_dir)
    }

    /// Execute this project's `run.js`, ensuring that everything runs through
    /// node or a browser correctly
    pub fn test(&mut self) {
        let (root, _target_dir) = self.gen_bindings();

        if !self.webpack {
            let mut cmd = Command::new("node");
            cmd.args(&self.node_args);
            if self.nodejs_experimental_modules {
                cmd.arg("--experimental-modules").arg("run.mjs");
            } else {
                cmd.arg("run.js");
            }
            cmd.current_dir(&root);
            run(&mut cmd, "node");
            return
        }

        // Generate typescript bindings for the wasm module
        {
            let _x = wrap_step("running wasm2es6js");
            let mut wasm = Vec::new();
            File::open(root.join("out_bg.wasm"))
                .unwrap()
                .read_to_end(&mut wasm)
                .unwrap();
            let obj = cli::wasm2es6js::Config::new()
                .base64(true)
                .generate(&wasm)
                .expect("failed to convert wasm to js");
            File::create(root.join("out_bg.d.ts"))
                .unwrap()
                .write_all(obj.typescript().as_bytes())
                .unwrap();
        }

        // move files from the root into each test, it looks like this may be
        // needed for webpack to work well when invoked concurrently.
        fs::hard_link("package.json", root.join("package.json")).unwrap();
        if !Path::new("node_modules").exists() {
            panic!("\n\nfailed to find `node_modules`, have you run `npm install` yet?\n\n");
        }
        let cwd = env::current_dir().unwrap();
        symlink_dir(&cwd.join("node_modules"), &root.join("node_modules")).unwrap();

        if self.headless {
            return self.test_headless(&root)
        }

        // Execute webpack to generate a bundle
        let mut cmd = self.npm();
        cmd.arg("run").arg("run-webpack").current_dir(&root);
        run(&mut cmd, "npm");

        let mut cmd = Command::new("node");
        cmd.args(&self.node_args);
        cmd.arg(root.join("bundle.js")).current_dir(&root);
        run(&mut cmd, "node");
    }

    fn npm(&self) -> Command {
        if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.arg("/c");
            c.arg("npm");
            c
        } else {
            Command::new("npm")
        }
    }

    fn test_headless(&mut self, root: &Path) {
        // Serialize all headless tests since they require starting
        // webpack-dev-server on the same port.
        lazy_static! {
            static ref MUTEX: Mutex<()> = Mutex::new(());
        }
        let _lock = MUTEX.lock().unwrap();

        let mut cmd = self.npm();
        cmd.arg("run")
            .arg("run-webpack-dev-server")
            .arg("--")
            .arg("--quiet")
            .arg("--watch-stdin")
            .current_dir(&root);
        let _server = run_in_background(&mut cmd, "webpack-dev-server".into());

        // wait for webpack-dev-server to come online and bind its port
        loop {
            if TcpStream::connect("127.0.0.1:8080").is_ok() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }

        let path = env::var_os("PATH").unwrap_or_default();
        let mut path = env::split_paths(&path).collect::<Vec<_>>();
        path.push(root.join("node_modules/geckodriver"));

        let mut cmd = Command::new("node");
        cmd.args(&self.node_args)
            .arg(root.join("run-headless.js"))
            .current_dir(&root)
            .env("PATH", env::join_paths(&path).unwrap());
        run(&mut cmd, "node");
    }

    /// Reads JS generated by `wasm-bindgen` to a string.
    pub fn read_js(&self) -> String {
        let path = root().join(if self.nodejs_experimental_modules {
            "out.mjs"
        } else {
            "out.js"
        });
        println!("js, {:?}", &path);
        fs::read_to_string(path).expect("Unable to read js")
    }
}

#[cfg(unix)]
fn symlink_dir(a: &Path, b: &Path) -> io::Result<()> {
    use std::os::unix::fs::symlink;
    symlink(a, b)
}

#[cfg(windows)]
fn symlink_dir(a: &Path, b: &Path) -> io::Result<()> {
    use std::os::windows::fs::symlink_dir;
    symlink_dir(a, b)
}

fn wrap_step(desc: &str) -> WrapStep {
    println!("···················································");
    println!("{}", desc);
    WrapStep { start: Instant::now() }
}

struct WrapStep { start: Instant }

impl Drop for WrapStep {
    fn drop(&mut self) {
        let dur = self.start.elapsed();
        println!(
            "dur: {}.{:03}s",
            dur.as_secs(),
            dur.subsec_nanos() / 1_000_000
        );
    }
}

pub fn run(cmd: &mut Command, program: &str) {
    let _x = wrap_step(&format!("running {:?}", cmd));
    let output = match cmd.output() {
        Ok(output) => output,
        Err(err) => panic!("failed to spawn `{}`: {}", program, err),
    };
    println!("exit: {}", output.status);
    if output.stdout.len() > 0 {
        println!("stdout ---\n{}", String::from_utf8_lossy(&output.stdout));
    }
    if output.stderr.len() > 0 {
        println!("stderr ---\n{}", String::from_utf8_lossy(&output.stderr));
    }
    assert!(output.status.success());
}

struct BackgroundChild {
    name: String,
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Option<thread::JoinHandle<io::Result<String>>>,
    stderr: Option<thread::JoinHandle<io::Result<String>>>,
}

impl Drop for BackgroundChild {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let status = self.child.wait().expect("failed to wait on child");
        let stdout = self
            .stdout
            .take()
            .unwrap()
            .join()
            .unwrap()
            .expect("failed to read stdout");
        let stderr = self
            .stderr
            .take()
            .unwrap()
            .join()
            .unwrap()
            .expect("failed to read stderr");

        println!("···················································");
        println!("background {}", self.name);
        println!("status: {}", status);
        println!("stdout ---\n{}", stdout);
        println!("stderr ---\n{}", stderr);
    }
}

fn run_in_background(cmd: &mut Command, name: String) -> BackgroundChild {
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.stdin(Stdio::piped());
    let mut child = cmd.spawn().expect(&format!("should spawn {} OK", name));
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let stdin = child.stdin.take().unwrap();
    let stdout = thread::spawn(move || {
        let mut t = String::new();
        stdout.read_to_string(&mut t)?;
        Ok(t)
    });
    let stderr = thread::spawn(move || {
        let mut t = String::new();
        stderr.read_to_string(&mut t)?;
        Ok(t)
    });
    BackgroundChild {
        name,
        child,
        stdout: Some(stdout),
        stderr: Some(stderr),
        stdin: Some(stdin),
    }
}
