use emp_modules::manifest::{
    ApiRequirement, Contribution, HOST_API_V3, HookPhase, HostApi, ManifestError, ModuleManifest,
    RouteAuth,
};
use serde_json::{Value, json};

const MANIFEST_JSON: &str = r#"{
    "id": "example-module",
    "module_version": "1.2.0",
    "host_api": {"major": 3, "min_minor": 0},
    "contributions": [
        {"type": "http_route", "id": "status", "method": "GET", "path": "/example/status", "auth": "local_proxy"},
        {"type": "hook", "phase": "project_catalog"},
        {"type": "catalog"},
        {"type": "panel", "id": "settings", "entry": "ui/settings.js", "revision": "1"}
    ]
}"#;

fn manifest() -> ModuleManifest {
    ModuleManifest::from_json(MANIFEST_JSON.as_bytes()).unwrap()
}

#[test]
fn route_ids_are_limited_to_nonempty_ascii_header_identifiers() {
    for id in [
        "",
        "line\nbreak",
        " leading",
        "trailing ",
        "route:part",
        &"x".repeat(65),
    ] {
        let mut manifest = manifest();
        let Contribution::HttpRoute { id: route_id, .. } = &mut manifest.contributions[0] else {
            panic!("missing HTTP route fixture");
        };
        *route_id = id.to_owned();
        assert_eq!(
            manifest.validate(HOST_API_V3),
            Err(vec![ManifestError::InvalidRouteId {
                index: 0,
                id: id.to_owned()
            }])
        );
    }
}

#[test]
fn manifest_round_trips_the_wire_contract() {
    let manifest = manifest();
    let encoded = serde_json::to_string(&manifest).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&encoded).unwrap(),
        serde_json::from_str::<Value>(MANIFEST_JSON).unwrap()
    );
    assert_eq!(
        ModuleManifest::from_json(encoded.as_bytes()).unwrap(),
        manifest
    );
    assert_eq!(manifest.validate(HOST_API_V3), Ok(()));
}

#[test]
fn hook_phases_map_to_the_host_to_module_methods() {
    for (phase, method) in [
        (HookPhase::Authenticate, "module.auth.authenticate"),
        (HookPhase::ProjectAccounts, "module.accounts.project"),
        (HookPhase::ResolveRoute, "module.route.resolve"),
        (HookPhase::CheckAccess, "module.access.check"),
        (HookPhase::ProjectCatalog, "module.catalog.project"),
        (HookPhase::CallFinished, "module.call.finished"),
    ] {
        assert_eq!(phase.method(), method);
    }
}

#[test]
fn host_api_requires_the_same_major_and_sufficient_minor() {
    let requirement = ApiRequirement {
        major: 3,
        min_minor: 1,
    };
    assert!(!requirement.accepts(HOST_API_V3));
    assert!(!requirement.accepts(HostApi { major: 4, minor: 1 }));
    assert!(requirement.accepts(HostApi { major: 3, minor: 1 }));
    assert!(requirement.accepts(HostApi { major: 3, minor: 2 }));
}

#[test]
fn manifest_rejects_unknown_fields_and_enum_values() {
    let original = serde_json::from_str::<Value>(MANIFEST_JSON).unwrap();
    for field in [
        "/extra",
        "/host_api/extra",
        "/contributions/0/extra",
        "/contributions/1/extra",
        "/contributions/2/extra",
        "/contributions/3/extra",
        "/contributions/0/auth",
        "/contributions/1/phase",
        "/contributions/2/type",
    ] {
        let (object, field) = field.rsplit_once('/').unwrap();
        let mut invalid = original.clone();
        invalid.pointer_mut(object).unwrap()[field] = json!("unknown");
        assert!(
            ModuleManifest::from_json(&serde_json::to_vec(&invalid).unwrap()).is_err(),
            "accepted {field} in {object}"
        );
    }
}

#[test]
fn manifest_rejects_repeated_id_and_host_api_major() {
    for (field, duplicate) in [
        (
            r#""id": "example-module""#,
            r#""id": "other", "id": "example-module""#,
        ),
        (r#""major": 3"#, r#""major": 2, "major": 3"#),
    ] {
        let input = MANIFEST_JSON.replace(field, duplicate);
        assert!(
            ModuleManifest::from_json(input.as_bytes()).is_err(),
            "{field}"
        );
    }
}

#[test]
fn manifest_requires_named_objects_and_string_tags() {
    let original = serde_json::from_str::<Value>(MANIFEST_JSON).unwrap();
    for (pointer, value) in [
        (
            "",
            json!(["example-module", "1.2.0", {"major": 3, "min_minor": 0}, []]),
        ),
        ("/host_api", json!([3, 0])),
        ("/contributions/2", json!(["catalog"])),
        ("/contributions/2", json!({})),
        ("/contributions/2/type", json!(0)),
        ("/contributions/0/auth", json!({"local_proxy": null})),
        ("/contributions/1/phase", json!({"project_catalog": null})),
    ] {
        let mut invalid = original.clone();
        *invalid.pointer_mut(pointer).unwrap() = value;
        assert!(
            ModuleManifest::from_json(&serde_json::to_vec(&invalid).unwrap()).is_err(),
            "accepted invalid shape at {pointer}: {invalid}"
        );
    }
}

#[test]
fn module_id_has_ascii_format_and_length_limits() {
    let mut manifest = manifest();
    for id in ["", "2module", "Example", "bad_id", "mé", &"a".repeat(33)] {
        manifest.id = id.into();
        assert_eq!(
            manifest.validate(HOST_API_V3),
            Err(vec![ManifestError::InvalidModuleId { id: id.into() }])
        );
    }
    for id in ["a".to_owned(), format!("m1-{}", "x".repeat(29))] {
        manifest.id = id;
        assert_eq!(manifest.validate(HOST_API_V3), Ok(()));
    }
}

#[test]
fn empty_module_version_is_rejected() {
    let mut manifest = manifest();
    manifest.module_version.clear();
    assert_eq!(
        manifest.validate(HOST_API_V3),
        Err(vec![ManifestError::EmptyModuleVersion])
    );
}

#[test]
fn incompatible_host_is_rejected() {
    let manifest = manifest();
    let host = HostApi { major: 2, minor: 9 };
    assert_eq!(
        manifest.validate(host),
        Err(vec![ManifestError::IncompatibleHostApi {
            required: manifest.host_api,
            host
        }])
    );
}

#[test]
fn ids_are_unique_across_route_and_panel_declarations() {
    for extra in [
        Contribution::HttpRoute {
            id: "status".into(),
            method: "POST".into(),
            path: "/example/status".into(),
            auth: RouteAuth::Management,
        },
        Contribution::Panel {
            id: "settings".into(),
            entry: "other.js".into(),
            revision: "2".into(),
        },
        Contribution::Panel {
            id: "status".into(),
            entry: "other.js".into(),
            revision: "2".into(),
        },
    ] {
        let id = match &extra {
            Contribution::HttpRoute { id, .. } | Contribution::Panel { id, .. } => id.clone(),
            _ => unreachable!(),
        };
        let mut manifest = manifest();
        manifest.contributions.push(extra);
        assert_eq!(
            manifest.validate(HOST_API_V3),
            Err(vec![ManifestError::DuplicateContributionId {
                index: 4,
                id
            }])
        );
    }
}

#[test]
fn route_methods_and_method_path_pairs_are_checked() {
    let mut manifest = manifest();
    for method in ["POST", "PUT", "PATCH", "DELETE"] {
        manifest.contributions.push(Contribution::HttpRoute {
            id: method.into(),
            method: method.into(),
            path: "/example/status".into(),
            auth: RouteAuth::Proxy,
        });
    }
    assert_eq!(manifest.validate(HOST_API_V3), Ok(()));
    for (method, expected) in [
        (
            "get",
            ManifestError::InvalidRouteMethod {
                index: 8,
                id: "extra".into(),
                method: "get".into(),
            },
        ),
        (
            "GET",
            ManifestError::DuplicateRoute {
                index: 8,
                id: "extra".into(),
                method: "GET".into(),
                path: "/example/status".into(),
            },
        ),
    ] {
        manifest.contributions.push(Contribution::HttpRoute {
            id: "extra".into(),
            method: method.into(),
            path: "/example/status".into(),
            auth: RouteAuth::Proxy,
        });
        assert_eq!(manifest.validate(HOST_API_V3), Err(vec![expected]));
        manifest.contributions.pop();
    }
}

#[test]
fn invalid_route_paths_are_rejected() {
    for path in ["relative", "/a..b", "/a//b", "/a?q", "/a#b"] {
        let mut manifest = manifest();
        manifest.contributions[0] = Contribution::HttpRoute {
            id: "status".into(),
            method: "GET".into(),
            path: path.into(),
            auth: RouteAuth::LocalProxy,
        };
        assert_eq!(
            manifest.validate(HOST_API_V3),
            Err(vec![ManifestError::InvalidRoutePath {
                index: 0,
                id: "status".into(),
                path: path.into()
            }])
        );
    }
}

#[test]
fn hooks_and_catalog_cannot_be_declared_twice() {
    for (extra, expected) in [
        (
            Contribution::Hook {
                phase: HookPhase::ProjectCatalog,
            },
            ManifestError::DuplicateHook {
                index: 4,
                phase: HookPhase::ProjectCatalog,
            },
        ),
        (
            Contribution::Catalog,
            ManifestError::DuplicateCatalog { index: 4 },
        ),
    ] {
        let mut manifest = manifest();
        manifest.contributions.push(extra);
        assert_eq!(manifest.validate(HOST_API_V3), Err(vec![expected]));
    }
}

#[test]
fn panel_requires_a_relative_js_entry_and_revision() {
    for (entry, valid) in [
        ("panel/main.js", true),
        ("panel/main..v2.js", true),
        ("a-b_c.d/x.js", true),
        ("..\t/main.js", false),
        ("./x.js", false),
        ("../ui.js", false),
        (".../x.js", false),
        ("a//b.js", false),
        ("https://example.org/panel.js", false),
        ("C:ui.js", false),
        ("/x.js", false),
        (r"\ui.js", false),
        (r"C:\ui.js", false),
        (r"a\b.js", false),
        ("x.js?v=1", false),
        ("中文.js", false),
        ("ui.css", false),
    ] {
        let mut manifest = manifest();
        manifest.contributions[3] = Contribution::Panel {
            id: "settings".into(),
            entry: entry.into(),
            revision: "1".into(),
        };
        let expected = if valid {
            Ok(())
        } else {
            Err(vec![ManifestError::InvalidPanelEntry {
                index: 3,
                id: "settings".into(),
                entry: entry.into(),
            }])
        };
        assert_eq!(manifest.validate(HOST_API_V3), expected, "{entry:?}");
    }
    let mut manifest = manifest();
    manifest.contributions[3] = Contribution::Panel {
        id: "settings".into(),
        entry: "ui.js".into(),
        revision: String::new(),
    };
    assert_eq!(
        manifest.validate(HOST_API_V3),
        Err(vec![ManifestError::EmptyPanelRevision {
            index: 3,
            id: "settings".into()
        }])
    );
}

#[test]
fn validation_reports_all_errors_across_the_manifest() {
    let mut manifest = manifest();
    manifest.module_version.clear();
    manifest.contributions[0] = Contribution::HttpRoute {
        id: "status".into(),
        method: "HEAD".into(),
        path: "status".into(),
        auth: RouteAuth::LocalProxy,
    };
    manifest.contributions.push(Contribution::Catalog);
    assert_eq!(
        manifest.validate(HOST_API_V3),
        Err(vec![
            ManifestError::EmptyModuleVersion,
            ManifestError::InvalidRouteMethod {
                index: 0,
                id: "status".into(),
                method: "HEAD".into()
            },
            ManifestError::InvalidRoutePath {
                index: 0,
                id: "status".into(),
                path: "status".into()
            },
            ManifestError::DuplicateCatalog { index: 4 },
        ])
    );
}
