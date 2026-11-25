use crate::support::modules::{BuiltModule, Fault, IDS, ModuleRelease};
#[cfg(target_os = "linux")]
use crate::support::modules::{ModuleEmp, eventually};
use crate::support::{
    Emp, NATIVE_TOKEN, Response, Upstream, UpstreamRequest, Workspace, http, write_file,
};
#[cfg(target_os = "linux")]
use emp_test_support::Deadline;
#[cfg(target_os = "linux")]
use emp_updater::worker::UPDATE_READY_WAIT;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
#[cfg(target_os = "linux")]
use std::{fs, path::Path, time::Duration};

const OLD_DEVICE_KEY: &str = "old-device-key";

fn old_gateway_client() -> Value {
    json!({"id":"old-device", "name":"Existing device", "created_at":1.0,
        "digest":format!("{:x}", Sha256::digest(OLD_DEVICE_KEY.as_bytes()))})
}

fn built_modules() -> Vec<BuiltModule> {
    IDS.into_iter().map(BuiltModule::current).collect()
}

struct Routes {
    first: Upstream,
    second: Upstream,
}

impl Routes {
    fn new() -> Self {
        let routes = Self {
            first: Upstream::start(),
            second: Upstream::start(),
        };
        for (id, upstream) in [("first", &routes.first), ("second", &routes.second)] {
            upstream.reply_json(&json!({"id":format!("resp-{id}"), "object":"response",
                "status":"completed", "model":"upstream-model", "output":[],
                "usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10}}));
        }
        routes
    }

    fn config(&self) -> Value {
        json!({"codex_base_url":self.first.base_url(), "auto_enable_on_start":false,
            "providers":[
                {"id":"first", "base_url":self.first.base_url(), "protocol":"responses",
                    "auth_mode":"api_key", "api_key":"first-api-key"},
                {"id":"second", "base_url":self.second.base_url(), "protocol":"responses",
                    "auth_mode":"api_key", "api_key":"second-api-key"}],
            "models":[
                {"id":"first/model", "provider":"first", "upstream_id":"upstream-model", "enabled":true},
                {"id":"second/model", "provider":"second", "upstream_id":"upstream-model", "enabled":true}]})
    }

    fn configured_workspace(&self, policy: &str) -> Workspace {
        let workspace = Workspace::new();
        let mut config = self.config();
        config["decision_provider"] = json!("first");
        for (index, id) in ["first", "second"].iter().enumerate() {
            config["providers"][index]["decision_api"] =
                json!({"protocol":"openai", "model":format!("{id}-decision")});
        }
        config["model_groups"] = json!([{"id":"auto/probe", "name_prefix":"auto",
            "policy":policy, "sources":["first/model","second/model"]}]);
        workspace.write_config(config);
        workspace
    }

    fn k2_workspace(&self, settings: &[&str]) -> Workspace {
        let workspace = Workspace::new();
        let mut config = self.config();
        let clients = workspace.root.join("state/gateway-clients.json");
        for setting in settings {
            match *setting {
                "decision_provider" => config["decision_provider"] = json!("first"),
                "decision_api" => {
                    config["providers"][0]["decision_api"] =
                        json!({"protocol":"openai","model":"first-decision"});
                }
                "gateway" => write_file(&clients, &json!([old_gateway_client()]).to_string()),
                #[cfg(target_os = "linux")]
                "gateway_encrypted" => {
                    let state = workspace.root.join("state");
                    emp_state::filesystem::create_private_directory(&state).unwrap();
                    let vault =
                        emp_state::VaultStore::from_sources(None, &state.join("master.key"))
                            .unwrap();
                    let mut client = old_gateway_client();
                    client["api_key"] = json!(OLD_DEVICE_KEY);
                    vault
                        .write_encrypted_json(&clients, &json!([client]))
                        .unwrap();
                }
                "aggregate" => {
                    config["model_groups"] = json!([{"id":"auto/probe",
                        "sources":["first/model","second/model"], "policy":"priority"}]);
                }
                "empty" => {
                    config["decision_provider"] = json!("");
                    config["model_groups"] = json!([]);
                    write_file(&clients, "[]");
                }
                _ => unreachable!("unknown K2 fixture setting {setting}"),
            }
        }
        workspace.write_config(config);
        workspace
    }

    fn assert_forwarded(&self, selected: &str, path: &str, model: &str) -> UpstreamRequest {
        let (upstream, other) = match selected {
            "first" => (&self.first, &self.second),
            "second" => (&self.second, &self.first),
            _ => panic!("unknown fixture provider {selected}"),
        };
        let forwarded = upstream.next_forwarded();
        assert_eq!(forwarded.method, "POST");
        assert_eq!(forwarded.path, path);
        assert_eq!(
            forwarded.headers["authorization"],
            format!("Bearer {selected}-api-key")
        );
        assert_eq!(
            forwarded.body.as_ref().expect("forwarded body")["model"],
            model
        );
        upstream.assert_idle();
        other.assert_idle();
        forwarded
    }

    fn assert_idle(&self) {
        self.first.assert_idle();
        self.second.assert_idle();
    }
}

fn routed(emp: &Emp, key: &str, model: &str, session: &str) -> Response {
    http(
        emp.port,
        "POST",
        "/v1/responses",
        &[
            ("Authorization", &format!("Bearer {key}")),
            ("Content-Type", "application/json"),
            ("session-id", session),
        ],
        json!({"model":model,"input":"module process fixture","stream":false})
            .to_string()
            .as_bytes(),
    )
}

fn report(emp: &Emp, path: &str) -> Value {
    let response = emp.get(path);
    assert_eq!(response.status, 200, "{path}: {}", response.text());
    response.json()
}

fn assert_ok(response: &Response, selected: &str) {
    assert_eq!(response.status, 200, "{}", response.text());
    assert_eq!(response.json()["id"], format!("resp-{selected}"));
}

fn exercise(emp: &Emp, module: &str, routes: &Routes) {
    match module {
        "decision" => {
            let response = emp.codex(
                "POST",
                "/v1/decisions",
                Some(&json!({"state":"lifecycle", "questions":{}})),
            );
            assert_ok(&response, "first");
            routes.assert_forwarded("first", "/v1/decisions", "first-decision");
        }
        "gateway" => {
            let client = emp.post("/api/gateway/clients", &json!({"name":"Lifecycle device"}));
            assert_eq!(client.status, 200, "{}", client.text());
            let client = client.json();
            assert_ok(
                &routed(
                    emp,
                    client["api_key"].as_str().expect("device key"),
                    "first/model",
                    "lifecycle-device",
                ),
                "first",
            );
            routes.assert_forwarded("first", "/v1/responses", "upstream-model");
        }
        "aggregate" => {
            assert_ok(
                &routed(emp, NATIVE_TOKEN, "auto/probe", "lifecycle-aggregate"),
                "first",
            );
            routes.assert_forwarded("first", "/v1/responses", "upstream-model");
        }
        _ => panic!("unknown module {module}"),
    }
}

fn assert_unavailable(emp: &Emp, module: &str, routes: &Routes) {
    let response = match module {
        "decision" => emp.codex("POST", "/v1/decisions", Some(&json!({"questions":{}}))),
        "gateway" => emp.get("/api/gateway/clients"),
        "aggregate" => routed(emp, NATIVE_TOKEN, "auto/probe", "lifecycle-aggregate"),
        _ => panic!("unknown module {module}"),
    };
    if module == "aggregate" {
        assert_eq!(response.status, 503, "{}", response.text());
        assert!(
            !report(emp, "/v1/models?client_version=0.169.0")["models"]
                .as_array()
                .expect("catalog")
                .iter()
                .any(|row| row["slug"] == "auto/probe")
        );
    } else {
        assert_eq!(response.status, 404, "{}", response.text());
        assert_eq!(response.json()["error"]["code"], "module_disabled");
    }
    routes.assert_idle();
}

#[test]
#[ignore = "needs current and next-version module binaries; see tests/support/modules.rs"]
fn m01_real_modules_install_update_preserve_on_failure_and_uninstall() {
    let mut modules = built_modules();
    let next: Vec<_> = IDS.into_iter().map(BuiltModule::next).collect();
    for (current, next) in modules.iter().zip(&next) {
        assert_ne!(
            current.version, next.version,
            "M01 needs a higher-version {} build",
            current.id
        );
        assert_ne!(
            current.digest, next.digest,
            "M01 needs different real artifacts"
        );
    }
    let release = ModuleRelease::start(&modules);
    let routes = Routes::new();
    let workspace = routes.configured_workspace("priority");
    let emp = release.start_emp(&workspace);
    let core_pid = emp.pid();
    for module in &modules {
        emp.change(&module.id, "install");
        let pid = emp.active(module);
        emp.change(&module.id, "enable");
        assert_eq!(emp.active(module), pid);
        exercise(&emp, &module.id, &routes);
    }
    for (index, candidate) in next.into_iter().enumerate() {
        let before: Vec<_> = modules.iter().map(|module| emp.active(module)).collect();
        release.publish(&candidate, Fault::None);
        let updates = emp.post("/api/modules", &json!({"action":"check_updates"}));
        assert_eq!(updates.status, 200, "{}", updates.text());
        let updates = updates.json();
        let offer = updates["updates"]
            .as_array()
            .expect("update offers")
            .iter()
            .find(|row| row["id"] == candidate.id)
            .expect("EMP_E2E_MODULE_NEXT_DIR must contain a higher-version build");
        assert_eq!(offer["latest_version"], candidate.version);
        let downloads = release.downloads();
        let updated = emp.post(
            "/api/modules",
            &json!({"id":candidate.id,
            "action":"update", "revision":offer["revision"]}),
        );
        assert_eq!(updated.status, 200, "{}", updated.text());
        assert!(
            release.downloads() > downloads,
            "update must download its real asset"
        );
        let replacement_pid = emp.active(&candidate);
        assert_ne!(replacement_pid, before[index]);
        emp.assert_gone(before[index]);
        modules[index] = candidate.clone();
        for (sibling, module) in modules
            .iter()
            .enumerate()
            .filter(|(sibling, _)| *sibling != index)
        {
            assert_eq!(
                emp.active(module),
                before[sibling],
                "unrelated worker changed"
            );
        }
        assert_eq!(emp.pid(), core_pid);
        exercise(&emp, &candidate.id, &routes);
        for (fault, code) in [
            (Fault::Digest, "checksum_mismatch"),
            (Fault::Executable, "module_start_failed"),
        ] {
            release.publish(&candidate, fault);
            let downloads = release.downloads();
            let failed = emp.post(
                "/api/modules",
                &json!({"id":candidate.id,"action":"update"}),
            );
            assert_eq!(failed.status, 409, "{}", failed.text());
            assert_eq!(failed.json()["error"]["code"], code);
            assert!(
                release.downloads() > downloads,
                "failure must reach the asset boundary"
            );
            assert_eq!(
                emp.active(&candidate),
                replacement_pid,
                "failed update displaced the working process"
            );
            for (sibling, module) in modules
                .iter()
                .enumerate()
                .filter(|(sibling, _)| *sibling != index)
            {
                assert_eq!(
                    emp.active(module),
                    before[sibling],
                    "failed update changed an unrelated worker"
                );
            }
            exercise(&emp, &candidate.id, &routes);
        }
        release.publish(&candidate, Fault::None);
    }
    for module in &modules {
        let pid = emp.active(module);
        let blocked = emp.post(
            "/api/modules",
            &json!({"id":module.id,"action":"uninstall"}),
        );
        assert_eq!(blocked.status, 409, "{}", blocked.text());
        assert_eq!(
            blocked.json()["error"]["code"],
            "module_disable_before_uninstall"
        );
        assert_eq!(emp.active(module), pid);
        emp.change(&module.id, "disable");
        emp.assert_gone(pid);
        assert_unavailable(&emp, &module.id, &routes);
        emp.change(&module.id, "uninstall");
        let row = emp.status(&module.id);
        assert_eq!(row["active"], false, "{row}");
        assert_eq!(row["enabled"], false, "{row}");
        assert!(row["installed_version"].is_null(), "{row}");
        assert_unavailable(&emp, &module.id, &routes);
    }
    assert_eq!(emp.pid(), core_pid);
    assert_ok(
        &routed(&emp, NATIVE_TOKEN, "first/model", "core-after-uninstall"),
        "first",
    );
    routes.assert_forwarded("first", "/v1/responses", "upstream-model");
    emp.stop();
    release
        .finish()
        .expect("module release server worker panicked");
}

#[test]
#[ignore = "needs current and next-version module binaries; see tests/support/modules.rs"]
fn m01_k2_normal_start_requires_install_and_restores_installed_modules_with_or_without_settings() {
    let modules = built_modules();
    let release = ModuleRelease::start(&modules);
    let routes = Routes::new();
    for settings in [
        &[][..],
        &["decision_provider"],
        &["decision_api"],
        &["gateway"],
        &["aggregate"],
    ] {
        let workspace = routes.k2_workspace(settings);
        let emp = release.start_emp(&workspace);
        for id in ["decision", "gateway", "aggregate", "team", "aero"] {
            let row = emp.status(id);
            assert_eq!(row["active"], false, "settings={settings:?}: {row}");
            assert_eq!(row["enabled"], false, "settings={settings:?}: {row}");
            assert!(
                row["installed_version"].is_null(),
                "settings={settings:?}: {row}"
            );
        }
        assert_ok(
            &routed(&emp, NATIVE_TOKEN, "first/model", "core-only"),
            "first",
        );
        routes.assert_forwarded("first", "/v1/responses", "upstream-model");
        for module in &modules {
            emp.change(&module.id, "install");
            emp.active(module);
        }
        if settings == ["gateway"] {
            assert_eq!(
                report(&emp, "/api/gateway/clients")["clients"][0]["id"],
                "old-device"
            );
            assert_ok(
                &routed(&emp, OLD_DEVICE_KEY, "first/model", "old-device"),
                "first",
            );
            routes.assert_forwarded("first", "/v1/responses", "upstream-model");
        }
        emp.stop();
        let restored = release.start_emp(&workspace);
        for module in &modules {
            restored.active(module);
        }
        for id in ["team", "aero"] {
            assert_eq!(restored.status(id)["active"], false);
        }
        restored.stop();
    }
    release
        .finish()
        .expect("module release server worker panicked");
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "needs embedded transition payloads and current and next Team builds; run cargo xtask module-e2e"]
fn m01_k2_bundled_update_restores_configured_or_enabled_modules() {
    let modules = built_modules();
    let team = BuiltModule::current("team");
    let next_team = BuiltModule::next("team");
    let release = ModuleRelease::start(&modules);
    release.publish(&next_team, Fault::None);
    let routes = Routes::new();
    let installation = Workspace::new();
    let built_core = Path::new(env!("CARGO_BIN_EXE_EMP"));
    let executable = installation.root.join("EMP");
    fs::copy(built_core, &executable).expect("copy packaged Core");
    let version = env!("CARGO_PKG_VERSION");
    assert_ne!(team.version, version, "Team must have its own version");
    assert_eq!(UPDATE_READY_WAIT.get(), Duration::from_secs(60));

    let cases: &[(&[&str], &[&str])] = &[
        (&[], &[]),
        (&["empty"], &[]),
        (&["decision_provider"], &["decision"]),
        (&["decision_api"], &["decision"]),
        (&["gateway"], &["gateway"]),
        (&["aggregate"], &["aggregate"]),
        (&["decision_api", "aggregate"], &["decision", "aggregate"]),
        (
            &[
                "decision_provider",
                "decision_api",
                "gateway_encrypted",
                "aggregate",
            ],
            &["decision", "gateway", "aggregate"],
        ),
    ];
    let legacy_install = |enabled| {
        json!({"enabled":enabled, "versions":{"0.14.2":"0".repeat(64)},
            "previous":{"version":"0.14.1", "digest":"1".repeat(64)}})
    };
    let cases = cases
        .iter()
        .map(|&(settings, expected)| (settings, expected, json!({}), false))
        .chain([
            (
                &["aggregate"][..],
                &["decision", "gateway", "aggregate"][..],
                json!({"decision":legacy_install(true), "gateway":legacy_install(true),
                    "aggregate":legacy_install(true), "team":{"enabled":true,
                        "versions":{"0.14.2":"0".repeat(64)},
                        "previous":{"digest":team.digest,"version":team.version}}}),
                false,
            ),
            (
                &[][..],
                &[][..],
                json!({"team":{"enabled":true,"versions":{(version):team.digest}}}),
                false,
            ),
            (
                &[][..],
                &[][..],
                json!({"decision":legacy_install(false)}),
                false,
            ),
            (
                &["gateway_encrypted"][..],
                &["gateway"][..],
                json!({}),
                true,
            ),
        ]);
    for (index, (settings, expected, mut expected_registry, damage_gateway)) in cases.enumerate() {
        let workspace = routes.k2_workspace(settings);
        let directory = workspace.root.join("state/modules");
        let registry_path = directory.join("modules.json");
        if expected_registry != json!({}) {
            write_file(&registry_path, &expected_registry.to_string());
        }
        let has_team = expected_registry.get("team").is_some();
        if has_team {
            fs::copy(
                built_core.with_file_name("empmod-team"),
                directory.join(format!("team-{}", team.digest)),
            )
            .expect("stage the installed Team artifact");
        }
        let clients = workspace.root.join("state/gateway-clients.json");
        let saved_clients = damage_gateway.then(|| {
            let saved = fs::read(&clients).unwrap();
            write_file(&clients, "{");
            saved
        });
        let job = installation.root.join(format!(".emp-update-m01-{index}"));
        let args = workspace.serve_arguments();
        let nonce = format!("m01-{index}");
        write_file(
            &job.join("plan.json"),
            &json!({"target":executable,"candidate":job.join("candidate"),"relative_binary":"",
                "parents":[],"args":args,"version":version,"nonce":nonce})
            .to_string(),
        );
        let ready = job.join("ready.json");
        let mut command = workspace.command_for(&executable);
        command.args(&args).env("EMP_UPDATE_READY", &ready);
        release.configure_command(&mut command);
        let deadline = Deadline::after(UPDATE_READY_WAIT.into());
        let emp = ModuleEmp::spawn(&workspace, command);
        let expected_receipt = json!({"version":version,"nonce":nonce});
        eventually(
            &deadline,
            "candidate Core did not acknowledge the update",
            || {
                let receipt: Value = serde_json::from_slice(&fs::read(&ready).ok()?).ok()?;
                (receipt == expected_receipt).then_some(())
            },
        );

        let registry: Value = serde_json::from_slice(&fs::read(registry_path).unwrap()).unwrap();
        let artifacts: Vec<_> = fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name != "modules.json")
            .collect();
        assert_eq!(
            artifacts.len(),
            expected.len() + usize::from(has_team),
            "settings={settings:?}: {artifacts:?}"
        );
        for module in &modules {
            if expected.contains(&module.id.as_str()) {
                if damage_gateway && module.id == "gateway" {
                    let row = emp.status("gateway");
                    assert_eq!(row["enabled"], true, "{row}");
                    assert_eq!(row["active"], false, "{row}");
                    assert_eq!(row["installed_version"], module.version, "{row}");
                    assert_eq!(row["error"], "invalid_gateway_clients", "{row}");
                } else {
                    emp.active(module);
                }
                let install = expected_registry
                    .as_object_mut()
                    .unwrap()
                    .entry(module.id.clone())
                    .or_insert_with(|| json!({"versions":{}}));
                install["enabled"] = json!(true);
                install["versions"][version] = json!(module.digest);
                install["artifact"] = json!({"digest":module.digest,"version":module.version});
            } else {
                let row = emp.status(&module.id);
                assert_eq!(row["active"], false, "{row}");
                assert_eq!(row["enabled"], false, "{row}");
                if expected_registry.get(&module.id).is_none() {
                    assert!(row["installed_version"].is_null(), "{row}");
                }
            }
        }
        assert_eq!(registry, expected_registry, "settings={settings:?}");
        let team_status = emp.status("team");
        assert_eq!(team_status["active"], has_team, "{team_status}");
        assert_eq!(emp.status("aero")["active"], false);
        if has_team {
            let rolled_back = expected_registry["team"].get("previous").is_some();
            let error = rolled_back.then_some("module_update_rolled_back");
            assert_eq!(team_status["error"], json!(error), "{team_status}");
            assert_eq!(
                team_status["installed_version"], team.version,
                "{team_status}"
            );
            assert_eq!(team_status["active_version"], team.version, "{team_status}");
            assert_eq!(team_status["active_revision"], team.digest, "{team_status}");
            let panel = emp.get("/assets/module-team.js");
            assert_eq!(panel.status, 200, "{}", panel.text());
            let updates = emp.post("/api/modules", &json!({"action":"check_updates"}));
            assert_eq!(updates.status, 200, "{}", updates.text());
            let updates = updates.json();
            let offer = updates["updates"]
                .as_array()
                .expect("update offers")
                .iter()
                .find(|row| row["id"] == "team")
                .expect("higher-version Team offer");
            assert_eq!(offer["current_version"], team.version);
            assert_eq!(offer["latest_version"], next_team.version);
        }
        if expected.is_empty() {
            assert_ok(
                &routed(&emp, NATIVE_TOKEN, "first/model", "core-only"),
                "first",
            );
            routes.assert_forwarded("first", "/v1/responses", "upstream-model");
        }
        if expected.contains(&"aggregate") {
            exercise(&emp, "aggregate", &routes);
        }
        if settings.contains(&"decision_provider") && settings.contains(&"decision_api") {
            exercise(&emp, "decision", &routes);
        }
        let emp = if let Some(saved) = saved_clients {
            assert_eq!(fs::read(&clients).unwrap(), b"{");
            emp.stop();
            fs::write(clients, saved).unwrap();
            let mut command = workspace.command_for(&executable);
            command.args(workspace.serve_arguments());
            let restarted = ModuleEmp::spawn(&workspace, command);
            restarted.active(
                modules
                    .iter()
                    .find(|module| module.id == "gateway")
                    .unwrap(),
            );
            restarted
        } else {
            emp
        };
        if settings.contains(&"gateway") || settings.contains(&"gateway_encrypted") {
            assert_ok(
                &routed(&emp, OLD_DEVICE_KEY, "first/model", "old-device"),
                "first",
            );
            routes.assert_forwarded("first", "/v1/responses", "upstream-model");
        }
        emp.stop();
    }
    release
        .finish()
        .expect("module release server worker panicked");
}

#[test]
#[ignore = "needs current and next-version module binaries; see tests/support/modules.rs"]
fn m02_decision_routes_to_default_and_overridden_real_services() {
    let module = BuiltModule::current("decision");
    let release = ModuleRelease::start(std::slice::from_ref(&module));
    let routes = Routes::new();
    let workspace = routes.configured_workspace("priority");
    let emp = release.start_emp(&workspace);
    emp.change("decision", "install");
    let pid = emp.active(&module);
    for (provider, explicit, selected, expected_model) in [
        (None, None, "first", "first-decision"),
        (Some("second"), None, "second", "second-decision"),
        (
            Some("first"),
            Some("explicit-decision"),
            "first",
            "explicit-decision",
        ),
    ] {
        let mut headers = vec![
            ("Authorization", format!("Bearer {NATIVE_TOKEN}")),
            ("Content-Type", "application/json".into()),
        ];
        if let Some(provider) = provider {
            headers.push(("X-EMP-Provider", provider.into()));
        }
        let mut body = json!({"state":"routing fixture","questions":{}});
        if let Some(model) = explicit {
            body["model"] = json!(model);
        }
        let response = http(
            emp.port,
            "POST",
            "/v1/decisions",
            &headers
                .iter()
                .map(|(key, value)| (*key, value.as_str()))
                .collect::<Vec<_>>(),
            body.to_string().as_bytes(),
        );
        assert_ok(&response, selected);
        routes.assert_forwarded(selected, "/v1/decisions", expected_model);
    }
    emp.change("decision", "disable");
    emp.assert_gone(pid);
    assert_unavailable(&emp, "decision", &routes);
    emp.change("decision", "enable");
    emp.active(&module);
    exercise(&emp, "decision", &routes);
    emp.stop();
    release
        .finish()
        .expect("module release server worker panicked");
}

#[test]
#[ignore = "needs current and next-version module binaries; see tests/support/modules.rs"]
fn m02_gateway_admits_device_keys_rejects_revoked_keys_and_attributes_usage() {
    let module = BuiltModule::current("gateway");
    let release = ModuleRelease::start(std::slice::from_ref(&module));
    let routes = Routes::new();
    let workspace = routes.configured_workspace("priority");
    let emp = release.start_emp(&workspace);
    emp.change("gateway", "install");
    let pid = emp.active(&module);
    let mut clients = Vec::new();
    let mut requests = Vec::new();
    for name in ["Desktop", "Laptop"] {
        let issued = emp.post("/api/gateway/clients", &json!({"name":name}));
        assert_eq!(issued.status, 200, "{}", issued.text());
        let client = issued.json();
        assert_ok(
            &routed(
                &emp,
                client["api_key"].as_str().expect("device key"),
                "first/model",
                name,
            ),
            "first",
        );
        requests.push(
            routes
                .assert_forwarded("first", "/v1/responses", "upstream-model")
                .headers["x-emp-request-id"]
                .clone(),
        );
        clients.push(client);
    }
    let usage = report(
        &emp,
        "/api/gateway/usage?group=device&start=0&end=4102444800",
    );
    let rows = usage["rows"].as_array().expect("device usage");
    assert_eq!(rows.len(), 2, "{usage}");
    let calls = report(&emp, "/api/calls?start=0&end=4102444800");
    for (client, request_id) in clients.iter().zip(&requests) {
        let row = rows
            .iter()
            .find(|row| row["caller_id"] == client["id"])
            .expect("usage attributed to issued device");
        assert_eq!(row["caller_name"], client["name"]);
        assert_eq!(row["calls"], 1, "{row}");
        assert_eq!(row["input_tokens"], 7, "{row}");
        assert_eq!(row["output_tokens"], 3, "{row}");
        assert_eq!(row["models"][0]["model"], "upstream-model");
        let call = calls["records"]
            .as_array()
            .expect("call records")
            .iter()
            .find(|row| row["request_id"] == *request_id)
            .expect("device call");
        assert_eq!(call["caller_id"], client["id"], "{call}");
        assert_eq!(call["caller_name"], client["name"]);
        assert_eq!(call["provider_id"], "first");
    }
    let removed = emp.api(
        "DELETE",
        "/api/gateway/clients",
        Some(&json!({"id":clients[0]["id"]})),
    );
    assert_eq!(removed.status, 200, "{}", removed.text());
    for key in ["wrong-device-key", clients[0]["api_key"].as_str().unwrap()] {
        let denied = routed(&emp, key, "first/model", "denied-device");
        assert_eq!(denied.status, 401, "{}", denied.text());
        routes.assert_idle();
    }
    let history = report(&emp, "/api/gateway/usage?start=0&end=4102444800");
    for client in &clients {
        let row = history["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["caller_id"] == client["id"])
            .expect("retained device usage");
        assert_eq!(
            row["calls"], 1,
            "denied requests must not add device usage: {row}"
        );
    }
    emp.change("gateway", "disable");
    emp.assert_gone(pid);
    assert_unavailable(&emp, "gateway", &routes);
    let denied = routed(
        &emp,
        clients[1]["api_key"].as_str().unwrap(),
        "first/model",
        "disabled-device",
    );
    assert_eq!(denied.status, 401, "{}", denied.text());
    routes.assert_idle();
    assert_ok(
        &routed(&emp, NATIVE_TOKEN, "first/model", "local-after-disable"),
        "first",
    );
    routes.assert_forwarded("first", "/v1/responses", "upstream-model");
    emp.stop();
    release
        .finish()
        .expect("module release server worker panicked");
}

#[test]
#[ignore = "needs current and next-version module binaries; see tests/support/modules.rs"]
fn m02_aggregate_selects_candidates_pins_sessions_and_disables_routes() {
    let module = BuiltModule::current("aggregate");
    let release = ModuleRelease::start(std::slice::from_ref(&module));
    let routes = Routes::new();
    for (policy, choices) in [
        ("priority", ["first", "first", "first", "first"]),
        ("round_robin", ["first", "second", "first", "second"]),
    ] {
        let workspace = routes.configured_workspace(policy);
        let emp = release.start_emp(&workspace);
        emp.change("aggregate", "install");
        let pid = emp.active(&module);
        for (session, selected) in ["session-a", "session-b", "session-a", "session-b"]
            .into_iter()
            .zip(choices)
        {
            assert_ok(&routed(&emp, NATIVE_TOKEN, "auto/probe", session), selected);
            routes.assert_forwarded(selected, "/v1/responses", "upstream-model");
        }
        emp.change("aggregate", "disable");
        emp.assert_gone(pid);
        assert_unavailable(&emp, "aggregate", &routes);
        for session in ["session-a", "session-b"] {
            let denied = routed(&emp, NATIVE_TOKEN, "auto/probe", session);
            assert_eq!(denied.status, 503, "{}", denied.text());
            routes.assert_idle();
        }
        assert_ok(
            &routed(&emp, NATIVE_TOKEN, "first/model", "direct-route"),
            "first",
        );
        routes.assert_forwarded("first", "/v1/responses", "upstream-model");
        emp.change("aggregate", "enable");
        emp.active(&module);
        assert_ok(
            &routed(&emp, NATIVE_TOKEN, "auto/probe", "session-b"),
            choices[1],
        );
        routes.assert_forwarded(choices[1], "/v1/responses", "upstream-model");
        emp.stop();
    }
    release
        .finish()
        .expect("module release server worker panicked");
}
