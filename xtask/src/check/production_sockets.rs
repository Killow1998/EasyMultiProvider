use super::{Result, test_scope::TestScopes};
use proc_macro2::{Delimiter, TokenStream, TokenTree};
use std::collections::BTreeSet;
use syn::{
    ext::IdentExt,
    visit::{self, Visit},
};

pub(super) fn check(
    path: &str,
    source: &str,
    scopes: &TestScopes,
    errors: &mut Vec<String>,
) -> Result {
    let Some((package, module)) = path
        .strip_prefix("crates/")
        .and_then(|path| path.split_once("/src/"))
    else {
        return Ok(());
    };
    if !module.ends_with(".rs")
        || matches!(package, "emp-transport" | "emp-test-support")
        || (package == "emp-app" && module.starts_with("server/"))
        || scopes.entire_file()
    {
        return Ok(());
    }
    let syntax = syn::parse_file(source).map_err(|error| format!("{path}: {error}"))?;
    let mut sockets = SocketUses::default();
    sockets.visit_file(&syntax);
    for (line, api) in sockets.0 {
        if !scopes.contains(line) {
            errors.push(format!(
                "[production-sockets] {path}:{line}: {api} 只能在连接层 server/ 或 emp-transport 使用；向业务层传递协议数据或取消信号"
            ));
        }
    }
    Ok(())
}

#[derive(Default)]
struct SocketUses(BTreeSet<(usize, String)>);

impl SocketUses {
    fn socket_type(&mut self, ident: &syn::Ident) {
        let api = ident.unraw().to_string();
        if matches!(
            api.as_str(),
            "TcpSocket"
                | "TcpListener"
                | "TcpStream"
                | "UdpSocket"
                | "UnixStream"
                | "UnixListener"
                | "UnixDatagram"
                | "SockRef"
                | "socket2"
                | "AsRawSocket"
                | "FromRawSocket"
                | "IntoRawSocket"
        ) {
            self.0.insert((ident.span().start().line, api));
        }
    }

    fn socket_method(&mut self, method: &syn::Ident, has_arguments: bool) {
        let api = method.unraw().to_string();
        if matches!(
            api.as_str(),
            "set_read_timeout"
                | "set_write_timeout"
                | "set_nodelay"
                | "set_keepalive"
                | "set_nonblocking"
                | "set_broadcast"
                | "local_addr"
                | "peer_addr"
                | "set_ttl"
                | "as_raw_socket"
                | "into_raw_socket"
                | "recv_from"
                | "send_to"
                | "socket"
        ) || (matches!(api.as_str(), "peek" | "shutdown" | "try_read" | "try_write")
            && has_arguments)
            || (api == "accept" && !has_arguments)
        {
            self.0.insert((method.span().start().line, api));
        }
    }

    fn macro_tokens(&mut self, tokens: TokenStream) {
        let tokens: Vec<_> = tokens.into_iter().collect();
        for token in &tokens {
            match token {
                TokenTree::Ident(ident) => self.socket_type(ident),
                TokenTree::Group(group) => self.macro_tokens(group.stream()),
                _ => {}
            }
        }
        for window in tokens.windows(3) {
            if let [
                TokenTree::Punct(dot),
                TokenTree::Ident(method),
                TokenTree::Group(args),
            ] = window
                && dot.as_char() == '.'
                && args.delimiter() == Delimiter::Parenthesis
            {
                self.socket_method(method, !args.stream().is_empty());
            }
        }
    }
}

impl<'ast> Visit<'ast> for SocketUses {
    fn visit_path_segment(&mut self, segment: &'ast syn::PathSegment) {
        self.socket_type(&segment.ident);
        visit::visit_path_segment(self, segment);
    }

    fn visit_use_path(&mut self, path: &'ast syn::UsePath) {
        self.socket_type(&path.ident);
        visit::visit_use_path(self, path);
    }

    fn visit_use_name(&mut self, name: &'ast syn::UseName) {
        self.socket_type(&name.ident);
    }

    fn visit_use_rename(&mut self, rename: &'ast syn::UseRename) {
        self.socket_type(&rename.ident);
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        self.socket_method(&call.method, !call.args.is_empty());
        visit::visit_expr_method_call(self, call);
    }

    fn visit_macro(&mut self, invocation: &'ast syn::Macro) {
        self.macro_tokens(invocation.tokens.clone());
        visit::visit_macro(self, invocation);
    }
}

#[cfg(test)]
mod tests {
    use super::{check as check_scoped, *};

    fn check(path: &str, source: &str, errors: &mut Vec<String>) -> Result {
        check_scoped(
            path,
            source,
            &super::super::test_scope::tests::fixture(path, source),
            errors,
        )
    }

    #[test]
    fn rejects_methods_on_an_inferred_listener_and_stream() {
        let source = r#"
async fn probe() {
    let listener = open_listener()?;
    listener.set_ttl(32)?;
    let _ = listener.local_addr()?;
    let (stream, _) = listener.accept().await?;
    stream.try_write(b"data")?;
}
fn unrelated() { state.try_read(); state.try_write(); }
"#;
        let mut errors = Vec::new();
        check("crates/emp-app/src/lifecycle/probe.rs", source, &mut errors).unwrap();
        assert_eq!(errors.len(), 4, "{errors:?}");
        for api in ["set_ttl", "local_addr", "accept", "try_write"] {
            assert!(
                errors
                    .iter()
                    .any(|error| error.contains(&format!("{api} 只能")))
            );
        }
    }

    #[test]
    fn rejects_socket_access_in_production_business_code() {
        let source = r#"
use tokio::net::TcpStream as Peer;
use socket2::SockRef;
fn probe() {
    peer.peek(&mut [0]);
    peer.set_read_timeout(None);
    peer.shutdown(std::net::Shutdown::Both);
    tokio::select! { bytes = peer.peek(&mut buffer) => bytes }
}
fn unrelated() { iterator.peek(); server.shutdown(); }
#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    fn fixture() { socket.set_write_timeout(None); }
}
"#;
        let mut errors = Vec::new();
        check("crates/emp-app/src/api/probe.rs", source, &mut errors).unwrap();
        assert_eq!(errors.len(), 7, "{errors:?}");
        for api in [
            "TcpStream",
            "SockRef",
            "peek",
            "set_read_timeout",
            "shutdown",
        ] {
            assert!(
                errors
                    .iter()
                    .any(|error| error.contains(&format!("{api} 只能")))
            );
        }
        for path in [
            "crates/emp-app/src/server/probe.rs",
            "crates/emp-transport/src/probe.rs",
            "crates/emp-app/src/services/probe/tests.rs",
        ] {
            let mut allowed = Vec::new();
            check(path, source, &mut allowed).unwrap();
            assert!(allowed.is_empty(), "{allowed:?}");
        }
    }
}
