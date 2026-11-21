use emp_modules::Module;
use serde::Serialize;

#[derive(Serialize)]
pub(super) struct Entry {
    copy_key: &'static str,
    icon: &'static str,
    toolbar: Option<Toolbar>,
    factory: Option<&'static str>,
    model_editor: bool,
}

#[derive(Serialize)]
struct Toolbar {
    slot: &'static str,
    id: &'static str,
    label: &'static str,
    icon: &'static str,
}

pub(super) fn entry(module: Module) -> Entry {
    let (copy_key, icon, toolbar, factory) = match module {
        Module::Decision => (
            "modules.decision_api",
            "module-decision",
            None,
            Some("createDecisionSettings"),
        ),
        Module::Gateway => (
            "modules.device_gateway",
            "module-gateway",
            None,
            Some("createGatewaySettings"),
        ),
        Module::Aggregate => (
            "modules.aggregate_routing",
            "module-aggregate",
            Some(Toolbar {
                slot: "model-routes",
                id: "catalog_aggregate_open",
                label: "aggregate.title",
                icon: "aggregate",
            }),
            Some("createAggregateSettings"),
        ),
        Module::Team => (
            "modules.team_workspace",
            "module-team",
            Some(Toolbar {
                slot: "workspace",
                id: "team_workspace_open",
                label: "modules.team_workspace",
                icon: "user",
            }),
            None,
        ),
        Module::Aero => (
            "modules.aero",
            "module-aero",
            Some(Toolbar {
                slot: "model-routes",
                id: "aero_open",
                label: "modules.aero",
                icon: "module-aero",
            }),
            Some("createAeroSettings"),
        ),
    };
    Entry {
        copy_key,
        icon,
        toolbar,
        factory,
        model_editor: module == Module::Aero,
    }
}
