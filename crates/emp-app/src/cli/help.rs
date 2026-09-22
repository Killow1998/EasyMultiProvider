//! Help text follows the existing Python command surface.
pub(super) fn text(command: Option<&str>) -> String {
    match command {
        Some("serve") => "usage: EMP serve [-h] [--config CONFIG] [--host HOST] [--port PORT]\n\noptions:\n  -h, --help       show this help message and exit\n  --config CONFIG  EMP JSON configuration path\n  --host HOST      override the local listen host\n  --port PORT      override the local listen port\n".to_owned(),
        Some(name @ ("doctor" | "restore")) => format!("usage: EMP {name} [-h] [--state-dir STATE_DIR] [--json]\n\noptions:\n  -h, --help            show this help message and exit\n  --state-dir STATE_DIR\n                        EMP local state directory (default: CODEX_HOME/easy-\n                        multi-provider/integration; relative explicit values\n                        resolve from cwd)\n  --json                print the operation result as JSON\n"),
        _ => "usage: EMP [-h] [--version] COMMAND ...\n\nEMP Runtime Control Plane commands\n\npositional arguments:\n  COMMAND\n    serve     start the existing EMP service\n    doctor    read Codex integration status without starting a service\n    restore   restore native Codex fields without starting a service\n\noptions:\n  -h, --help  show this help message and exit\n  --version   show program's version number and exit\n".to_owned(),
    }
}
