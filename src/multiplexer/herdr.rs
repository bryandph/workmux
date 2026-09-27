//! Local Herdr backend. Workmux owns Git; Herdr owns terminals.
//!
//! Only the small JSON API surface used here is decoded. No generated client,
//! server lifecycle management, worktree creation/removal, or automatic retries.

use anyhow::{Context, Result, bail};
use base64::Engine;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{Multiplexer, types::*, util};
use crate::config::SplitDirection;
use crate::shell::shell_quote;

const REQUEST_ID: &str = "workmux";
const MAX_RESPONSE: u64 = 8 * 1024 * 1024;

#[derive(Debug, Deserialize, thiserror::Error)]
#[error("Herdr {code}: {message}")]
struct ApiError {
    code: String,
    message: String,
}

#[derive(Debug, Deserialize)]
struct Envelope {
    id: String,
    result: Option<Value>,
    error: Option<ApiError>,
}

#[derive(Debug, Deserialize)]
struct Workspace {
    workspace_id: String,
    label: String,
    #[serde(default)]
    tokens: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct Pane {
    pane_id: String,
    workspace_id: String,
    cwd: PathBuf,
    foreground_cwd: Option<PathBuf>,
    terminal_title: Option<String>,
}

/// One explicitly addressed local server. Missing context never selects the
/// user's default/focused server implicitly.
#[derive(Debug)]
pub struct HerdrBackend {
    socket: Option<PathBuf>,
    bin: String,
}

impl Default for HerdrBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl HerdrBackend {
    pub fn new() -> Self {
        Self {
            socket: std::env::var_os("HERDR_SOCKET_PATH")
                .filter(|s| !s.is_empty())
                .map(PathBuf::from),
            bin: std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into()),
        }
    }

    pub fn for_socket(socket: &str) -> Self {
        Self {
            socket: Some(socket.into()),
            ..Self::new()
        }
    }

    fn socket(&self) -> Result<&Path> {
        let path = self.socket.as_deref().context(
            "Herdr requires HERDR_SOCKET_PATH; run workmux inside the intended Herdr session",
        )?;
        if !path.is_absolute() {
            bail!("HERDR_SOCKET_PATH must be absolute");
        }
        Ok(path)
    }

    fn request(&self, method: &str, params: Value) -> Result<Value> {
        let mut stream = UnixStream::connect(self.socket()?)
            .with_context(|| format!("connect to Herdr for {method}"))?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;
        let request = json!({"id":REQUEST_ID,"method":method,"params":params});
        serde_json::to_writer(&mut stream, &request)?;
        stream.write_all(b"\n")?;
        let mut line = String::new();
        BufReader::new(stream.take(MAX_RESPONSE + 1)).read_line(&mut line)
            .with_context(|| format!("read Herdr {method} response (operation may have completed; inspect before retrying)"))?;
        if line.len() as u64 > MAX_RESPONSE || !line.ends_with('\n') {
            bail!("Herdr {method}: oversized or incomplete response");
        }
        let envelope: Envelope = serde_json::from_str(&line)
            .with_context(|| format!("decode Herdr {method} response"))?;
        if envelope.id != REQUEST_ID {
            bail!("Herdr {method}: response ID mismatch");
        }
        match (envelope.result, envelope.error) {
            (Some(result), None) => Ok(result),
            (None, Some(error)) => Err(error.into()),
            _ => bail!("Herdr {method}: expected exactly one result or error"),
        }
    }

    fn field<T: DeserializeOwned>(result: Value, field: &str) -> Result<T> {
        serde_json::from_value(
            result
                .get(field)
                .cloned()
                .with_context(|| format!("Herdr response missing {field}"))?,
        )
        .with_context(|| format!("invalid Herdr {field}"))
    }

    fn workspaces(&self) -> Result<Vec<Workspace>> {
        Self::field(self.request("workspace.list", json!({}))?, "workspaces")
    }

    fn panes(&self) -> Result<Vec<Pane>> {
        Self::field(self.request("pane.list", json!({}))?, "panes")
    }

    fn pane(&self, id: &str) -> Result<Pane> {
        Self::field(self.request("pane.get", json!({"pane_id":id}))?, "pane")
    }

    fn workspace_for_name(&self, name: &str) -> Result<String> {
        let matches: Vec<_> = self
            .workspaces()?
            .into_iter()
            .filter(|w| w.label == name)
            .collect();
        match matches.as_slice() {
            [workspace] => Ok(workspace.workspace_id.clone()),
            [] => bail!("no Herdr workspace named {name:?}"),
            _ => bail!("ambiguous Herdr workspace name {name:?}; use a stable target"),
        }
    }

    fn workspace_for_target(&self, target: &WindowTarget) -> Result<String> {
        match &target.window_id {
            Some(id) => Ok(id.clone()),
            None => self.workspace_for_name(&target.full_name),
        }
    }

    fn workspace_for_pane_or_window(&self, id: &str) -> Result<String> {
        if self.workspaces()?.iter().any(|w| w.workspace_id == id) {
            Ok(id.into())
        } else {
            Ok(self.pane(id)?.workspace_id)
        }
    }

    fn close_workspace(&self, id: &str) -> Result<()> {
        self.request(
            "workspace.close",
            json!({"workspace_id":id,"close_group":false}),
        )?;
        Ok(())
    }

    fn no_sessions<T>() -> Result<T> {
        bail!("Herdr supports workmux window mode only")
    }

    fn cli_command(&self, args: &[&str]) -> Result<String> {
        let mut words = vec![
            "env".into(),
            shell_quote(&format!("HERDR_SOCKET_PATH={}", self.socket()?.display())),
            shell_quote(&self.bin),
        ];
        words.extend(args.iter().map(|arg| shell_quote(arg)));
        Ok(words.join(" "))
    }

    fn run_in_shell(&self, pane_id: &str, cwd: &Path, command: Option<&str>) -> Result<()> {
        // Used only for freshly created shell panes by setup_panes. The regular
        // agent submission path below validates agent identity instead.
        let shell = self.get_default_shell()?;
        let fallback = format!("exec {} -l", shell_quote(&shell));
        let script = format!(
            "cd {} && {}",
            shell_quote(&cwd.to_string_lossy()),
            command.unwrap_or(&fallback)
        );
        // Fresh panes may use Nushell or fish. A fixed, single-quoted bootstrap
        // leaves their parser no POSIX operators, substitutions, or user text.
        // Decode inside sh, and eval there so an exec retains the pane's PTY.
        let encoded = base64::engine::general_purpose::STANDARD.encode(script);
        self.send_keys(
            pane_id,
            &format!("sh -c 'eval \"$(printf %s {encoded} | base64 -d)\"'"),
        )
    }

    fn live_info(&self, pane: Pane, workspaces: &[Workspace]) -> Result<LivePaneInfo> {
        let workspace = workspaces
            .iter()
            .find(|w| w.workspace_id == pane.workspace_id)
            .context("Herdr pane references a missing workspace; retry observation")?;
        let mut info = util::build_live_pane_info(
            None,
            None,
            pane.foreground_cwd.unwrap_or(pane.cwd),
            pane.terminal_title.as_deref().unwrap_or(""),
            self.resolve_instance_id()?,
            workspace.label.clone(),
        );
        info.window_id = Some(workspace.workspace_id.clone());
        info.session_id = Some(self.resolve_instance_id()?);
        Ok(info)
    }
}

impl Multiplexer for HerdrBackend {
    fn name(&self) -> &'static str {
        "herdr"
    }

    fn is_running(&self) -> Result<bool> {
        self.request("ping", json!({}))?;
        Ok(true)
    }

    fn current_pane_id(&self) -> Option<String> {
        // A backend reconstructed for another instance must not inherit the
        // calling pane from this instance.
        let inherited = std::env::var_os("HERDR_SOCKET_PATH").map(PathBuf::from);
        (inherited == self.socket)
            .then(|| std::env::var("HERDR_PANE_ID").ok())
            .flatten()
    }

    fn active_pane_id(&self) -> Option<String> {
        Self::field::<Pane>(self.request("pane.current", json!({})).ok()?, "pane")
            .ok()
            .map(|p| p.pane_id)
    }

    fn current_window_id(&self) -> Result<Option<String>> {
        self.current_pane_id()
            .map(|id| self.pane(&id).map(|p| p.workspace_id))
            .transpose()
    }

    fn get_client_active_pane_path(&self) -> Result<PathBuf> {
        let pane: Pane = Self::field(self.request("pane.current", json!({}))?, "pane")?;
        Ok(pane.foreground_cwd.unwrap_or(pane.cwd))
    }

    fn create_window(&self, params: CreateWindowParams) -> Result<String> {
        let cwd = params.cwd.canonicalize()?;
        let label = util::prefixed(params.prefix, params.name);
        let root = crate::git::get_main_worktree_root_in(Some(&cwd))?.canonicalize()?;
        let checkout = PathBuf::from(
            crate::cmd::Cmd::new("git")
                .workdir(&cwd)
                .args(&["rev-parse", "--show-toplevel"])
                .run_and_capture_stdout()?,
        )
        .canonicalize()?;
        let result = if checkout != root {
            // Attach only. Git checkout creation, hooks, merge and removal stay
            // entirely in workmux's existing workflow.
            self.request(
                "worktree.open",
                json!({
                    "cwd":root,"path":checkout,"label":label,"focus":false
                }),
            )?
        } else {
            self.request(
                "workspace.create",
                json!({"cwd":cwd,"label":label,"focus":false}),
            )?
        };
        if result.get("already_open").and_then(Value::as_bool) == Some(true) {
            bail!("Herdr already has this worktree open; refusing to reinitialize its live panes");
        }
        let pane: Pane = Self::field(result, "root_pane")?;
        if pane.cwd != cwd {
            self.run_in_shell(&pane.pane_id, &cwd, None)?;
        }
        Ok(pane.pane_id)
    }

    fn supports_window_ownership(&self) -> bool {
        true
    }

    fn set_window_ownership(&self, id: &str, token: &str, is_primary: bool) -> Result<()> {
        let workspace = self.workspace_for_pane_or_window(id)?;
        self.request(
            "workspace.report_metadata",
            json!({
                "workspace_id":workspace,"source":"workmux",
                "tokens":{"workmux_owner":token,"workmux_primary":is_primary.to_string()}
            }),
        )?;
        Ok(())
    }

    fn owned_window_targets(&self, token: &str) -> Result<Vec<OwnedWindowTarget>> {
        Ok(self
            .workspaces()?
            .into_iter()
            .filter(|w| w.tokens.get("workmux_owner").is_some_and(|t| t == token))
            .map(|w| OwnedWindowTarget {
                is_primary: w.tokens.get("workmux_primary").is_some_and(|s| s == "true"),
                target: WindowTarget::with_id(w.label, None, w.workspace_id),
            })
            .collect())
    }

    fn owned_window_tokens(&self) -> Result<HashSet<String>> {
        Ok(self
            .workspaces()?
            .into_iter()
            .filter_map(|mut w| w.tokens.remove("workmux_owner"))
            .collect())
    }

    fn window_ownership_records(&self) -> Result<Vec<WindowOwnershipRecord>> {
        let panes = self.panes()?;
        self.workspaces()?
            .into_iter()
            .map(|mut w| {
                let pane_path = panes
                    .iter()
                    .find(|p| p.workspace_id == w.workspace_id)
                    .context("Herdr workspace has no pane for ownership recovery")?
                    .cwd
                    .clone();
                Ok(WindowOwnershipRecord {
                    window_id: w.workspace_id,
                    window_name: w.label,
                    session_name: self.instance_id(),
                    token: w.tokens.remove("workmux_owner"),
                    pane_path,
                })
            })
            .collect()
    }

    fn create_session(&self, _: CreateSessionParams) -> Result<String> {
        Self::no_sessions()
    }
    fn switch_to_session(&self, _: &str, _: &str) -> Result<()> {
        Self::no_sessions()
    }
    fn schedule_session_close(&self, _: &str, _: Duration) -> Result<()> {
        Self::no_sessions()
    }
    fn wait_until_session_closed(&self, _: &str) -> Result<()> {
        Self::no_sessions()
    }
    fn shell_switch_session_cmd(&self, _: &str) -> Result<String> {
        Self::no_sessions()
    }
    fn shell_kill_session_cmd(&self, _: &str) -> Result<String> {
        Self::no_sessions()
    }

    fn kill_window(&self, name: &str) -> Result<()> {
        self.close_workspace(&self.workspace_for_name(name)?)
    }
    fn kill_window_target(&self, target: &WindowTarget) -> Result<()> {
        self.close_workspace(&self.workspace_for_target(target)?)
    }
    fn rename_window(&self, old: &str, new: &str) -> Result<()> {
        let id = self.workspace_for_name(old)?;
        self.request("workspace.rename", json!({"workspace_id":id,"label":new}))?;
        Ok(())
    }
    fn rename_window_at_pane(&self, pane_id: &str, new: &str) -> Result<()> {
        let id = self.pane(pane_id)?.workspace_id;
        self.request("workspace.rename", json!({"workspace_id":id,"label":new}))?;
        Ok(())
    }
    fn schedule_window_close(&self, name: &str, delay: Duration) -> Result<()> {
        self.run_deferred_script(&format!(
            "sleep {}; {}",
            delay.as_secs_f64(),
            self.shell_kill_window_cmd(name)?
        ))
    }
    fn run_deferred_script(&self, script: &str) -> Result<()> {
        util::run_detached_sh_c(script)
    }
    fn shell_select_window_cmd(&self, name: &str) -> Result<String> {
        self.cli_command(&["workspace", "focus", &self.workspace_for_name(name)?])
    }
    fn shell_kill_window_cmd(&self, name: &str) -> Result<String> {
        self.cli_command(&["workspace", "close", &self.workspace_for_name(name)?])
    }
    fn shell_kill_window_target_cmd(&self, target: &WindowTarget) -> Result<String> {
        self.cli_command(&["workspace", "close", &self.workspace_for_target(target)?])
    }
    fn shell_close_window_by_id_guard_cmd(&self, id: &str) -> Result<String> {
        self.cli_command(&["workspace", "close", id])
    }
    fn select_window(&self, prefix: &str, name: &str) -> Result<()> {
        let id = self.workspace_for_name(&util::prefixed(prefix, name))?;
        self.request("workspace.focus", json!({"workspace_id":id}))?;
        Ok(())
    }
    fn select_window_target(&self, target: &WindowTarget) -> Result<()> {
        let id = self.workspace_for_target(target)?;
        self.request("workspace.focus", json!({"workspace_id":id}))?;
        Ok(())
    }
    fn window_target_exists(&self, target: &WindowTarget) -> Result<bool> {
        Ok(self.workspaces()?.iter().any(|w| match &target.window_id {
            Some(id) => &w.workspace_id == id,
            None => w.label == target.full_name,
        }))
    }
    fn current_window_name(&self) -> Result<Option<String>> {
        let Some(id) = self.current_window_id()? else {
            return Ok(None);
        };
        Ok(self
            .workspaces()?
            .into_iter()
            .find(|w| w.workspace_id == id)
            .map(|w| w.label))
    }
    fn get_all_window_names(&self) -> Result<HashSet<String>> {
        Ok(self.workspaces()?.into_iter().map(|w| w.label).collect())
    }
    fn select_pane(&self, id: &str) -> Result<()> {
        self.request("pane.focus", json!({"pane_id":id}))?;
        Ok(())
    }
    fn switch_to_pane(&self, id: &str, _: Option<&str>) -> Result<()> {
        self.select_pane(id)
    }
    fn zoom_pane(&self, id: &str) -> Result<()> {
        self.request("pane.zoom", json!({"pane_id":id,"mode":"on"}))?;
        Ok(())
    }
    fn kill_pane(&self, id: &str) -> Result<()> {
        self.request("pane.close", json!({"pane_id":id}))?;
        Ok(())
    }
    fn respawn_pane(&self, id: &str, cwd: &Path, cmd: Option<&str>) -> Result<String> {
        self.run_in_shell(id, cwd, cmd)?;
        Ok(id.into())
    }
    fn set_pane_name(&self, id: &str, name: &str) -> Result<()> {
        self.request("pane.rename", json!({"pane_id":id,"label":name}))?;
        Ok(())
    }
    fn capture_pane(&self, id: &str, lines: u16) -> Option<String> {
        let result = self
            .request(
                "pane.read",
                json!({
                    "pane_id":id,"source":"recent_unwrapped","lines":lines,"format":"text"
                }),
            )
            .ok()?;
        result.get("read")?.get("text")?.as_str().map(str::to_owned)
    }
    fn send_text_fragment(&self, id: &str, text: &str) -> Result<()> {
        self.request("pane.send_input", json!({"pane_id":id,"text":text}))?;
        Ok(())
    }
    fn send_enter(&self, id: &str) -> Result<()> {
        self.send_key(id, "enter")
    }
    fn send_key(&self, id: &str, key: &str) -> Result<()> {
        self.request("pane.send_input", json!({"pane_id":id,"keys":[key]}))?;
        Ok(())
    }
    fn send_keys(&self, id: &str, text: &str) -> Result<()> {
        self.request(
            "pane.send_input",
            json!({"pane_id":id,"text":text,"keys":["enter"]}),
        )?;
        Ok(())
    }
    fn paste_text(&self, id: &str, text: &str) -> Result<()> {
        self.send_text_fragment(id, text)
    }
    fn paste_and_submit(&self, id: &str, text: &str) -> Result<()> {
        self.send_keys(id, text)
    }
    fn send_prompt_to_agent(&self, id: &str, text: &str, _: Option<&str>) -> Result<()> {
        // Do not fall back to terminal injection on blocked/unknown agents or
        // older servers: a failure must not become input into an approval UI.
        self.request("agent.prompt", json!({"target":id,"text":text}))?;
        Ok(())
    }
    fn send_keys_to_agent(&self, id: &str, text: &str, agent: Option<&str>) -> Result<()> {
        self.send_prompt_to_agent(id, text, agent)
    }
    fn set_status(&self, _: &str, _: &str, _: bool) -> Result<()> {
        // Decorative status is deliberately separate from pane labels.
        Ok(())
    }
    fn clear_status(&self, _: &str) -> Result<()> {
        // Acknowledging workmux's badge must not erase Herdr lifecycle authority.
        Ok(())
    }
    fn ensure_status_format(&self, _: &str) -> Result<()> {
        Ok(())
    }
    fn split_pane(
        &self,
        target: &str,
        direction: &SplitDirection,
        cwd: &Path,
        size: Option<u16>,
        percentage: Option<u8>,
        command: Option<&str>,
    ) -> Result<String> {
        if size.is_some() {
            bail!("Herdr supports percentage split sizes, not cell sizes");
        }
        let direction = match direction {
            SplitDirection::Horizontal => "right",
            SplitDirection::Vertical => "down",
            SplitDirection::Stacked => bail!("Herdr does not support stacked splits"),
        };
        let mut params =
            json!({"target_pane_id":target,"direction":direction,"cwd":cwd,"focus":false});
        if let Some(percent) = percentage {
            params["ratio"] = json!(f64::from(percent) / 100.0);
        }
        let pane: Pane = Self::field(self.request("pane.split", params)?, "pane")?;
        if let Some(command) = command {
            self.run_in_shell(&pane.pane_id, cwd, Some(command))?;
        }
        Ok(pane.pane_id)
    }
    fn instance_id(&self) -> String {
        self.socket
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
    fn resolve_instance_id(&self) -> Result<String> {
        Ok(self.socket()?.to_string_lossy().into_owned())
    }
    fn get_live_pane_info(&self, id: &str) -> Result<Option<LivePaneInfo>> {
        // A complete successful inventory proves absence; transport failures and
        // malformed records must never purge live agents from the state store.
        let panes = self.panes()?;
        let Some(pane) = panes.into_iter().find(|p| p.pane_id == id) else {
            return Ok(None);
        };
        Ok(Some(self.live_info(pane, &self.workspaces()?)?))
    }
    fn get_all_live_pane_info(&self) -> Result<HashMap<String, LivePaneInfo>> {
        let panes = self.panes()?;
        let workspaces = self.workspaces()?;
        panes
            .into_iter()
            .map(|p| Ok((p.pane_id.clone(), self.live_info(p, &workspaces)?)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::thread;

    fn server(
        responses: Vec<Value>,
    ) -> (
        HerdrBackend,
        thread::JoinHandle<Vec<Value>>,
        tempfile::TempDir,
    ) {
        // macOS sockaddr_un has a short path limit.
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let socket = dir.path().join("api.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let worker = thread::spawn(move || {
            let mut requests = Vec::new();
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                requests.push(serde_json::from_str(&line).unwrap());
                serde_json::to_writer(&mut stream, &response).unwrap();
                stream.write_all(b"\n").unwrap();
            }
            requests
        });
        (
            HerdrBackend::for_socket(socket.to_str().unwrap()),
            worker,
            dir,
        )
    }

    fn ok(result: Value) -> Value {
        json!({"id":REQUEST_ID,"result":result})
    }

    #[test]
    fn blocked_agent_prompt_fails_without_terminal_fallback() {
        let (backend, worker, _dir) = server(vec![json!({
            "id":REQUEST_ID,"error":{"code":"agent_blocked","message":"approval required"}
        })]);
        let err = backend
            .send_prompt_to_agent("w1:p2", "do work\nthen test", Some("claude"))
            .unwrap_err();
        assert_eq!(
            err.downcast_ref::<ApiError>().unwrap().code,
            "agent_blocked"
        );
        let requests = worker.join().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["method"], "agent.prompt");
        assert_eq!(requests[0]["params"]["target"], "w1:p2");
        assert_eq!(requests[0]["params"]["text"], "do work\nthen test");
    }

    #[test]
    fn raw_submission_is_one_ordered_request() {
        let (backend, worker, _dir) = server(vec![ok(json!({"type":"ok"}))]);
        backend.send_keys("w1:p2", "echo 'a b'").unwrap();
        let requests = worker.join().unwrap();
        assert_eq!(requests[0]["method"], "pane.send_input");
        assert_eq!(
            requests[0]["params"],
            json!({"pane_id":"w1:p2","text":"echo 'a b'","keys":["enter"]})
        );
    }

    #[test]
    fn capture_decodes_read_result_and_uses_wire_enum() {
        let (backend, worker, _dir) = server(vec![ok(json!({
            "type":"pane_read","read":{"text":"captured output"}
        }))]);
        assert_eq!(
            backend.capture_pane("w1:p2", 50).as_deref(),
            Some("captured output")
        );
        assert_eq!(
            worker.join().unwrap()[0]["params"]["source"],
            "recent_unwrapped"
        );
    }

    #[test]
    fn all_mutations_decode_server_errors() {
        let (backend, worker, _dir) = server(vec![json!({
            "id":REQUEST_ID,"error":{"code":"workspace_group_close_required","message":"linked children"}
        })]);
        assert!(backend.close_workspace("w1").is_err());
        let requests = worker.join().unwrap();
        assert_eq!(requests[0]["params"]["close_group"], false);
    }

    #[test]
    fn mismatched_id_and_ambiguous_envelope_are_errors() {
        let (backend, worker, _dir) = server(vec![
            json!({"id":"someone-else","result":{"type":"pong"}}),
            json!({"id":REQUEST_ID,"result":{},"error":{"code":"bad","message":"bad"}}),
        ]);
        assert!(
            backend
                .is_running()
                .unwrap_err()
                .to_string()
                .contains("ID mismatch")
        );
        assert!(
            backend
                .is_running()
                .unwrap_err()
                .to_string()
                .contains("exactly one")
        );
        worker.join().unwrap();
    }

    #[test]
    fn malformed_inventory_is_not_an_absent_agent() {
        let (backend, worker, _dir) = server(vec![ok(json!({"panes":[{"pane_id":"w1:p1"}]}))]);
        assert!(backend.get_live_pane_info("w1:p1").is_err());
        worker.join().unwrap();
    }

    #[test]
    fn complete_inventory_can_prove_absence() {
        let (backend, worker, _dir) = server(vec![ok(json!({"panes":[]}))]);
        assert!(backend.get_live_pane_info("w1:p1").unwrap().is_none());
        worker.join().unwrap();
    }

    #[test]
    fn decorative_status_does_not_mutate_native_agent_state_or_pane_names() {
        let backend = HerdrBackend {
            socket: None,
            bin: "herdr".into(),
        };
        backend.set_status("w1:p1", "?", true).unwrap();
        backend.clear_status("w1:p1").unwrap();
    }

    #[test]
    fn duplicate_labels_are_not_arbitrarily_resolved() {
        let (backend, worker, _dir) = server(vec![ok(json!({"workspaces":[
            {"workspace_id":"w1","label":"wm-task","cwd":"/one"},
            {"workspace_id":"w2","label":"wm-task","cwd":"/two"}
        ]}))]);
        assert!(
            backend
                .workspace_for_name("wm-task")
                .unwrap_err()
                .to_string()
                .contains("ambiguous")
        );
        worker.join().unwrap();
    }

    #[test]
    fn owned_target_closes_by_id_not_label() {
        let (backend, worker, _dir) = server(vec![ok(json!({}))]);
        backend
            .kill_window_target(&WindowTarget::with_id(
                "duplicate-label".into(),
                None,
                "w2".into(),
            ))
            .unwrap();
        let requests = worker.join().unwrap();
        assert_eq!(requests[0]["params"]["workspace_id"], "w2");
    }

    #[test]
    fn deferred_commands_quote_binary_socket_and_target() {
        let backend = HerdrBackend {
            socket: Some("/tmp/my socket;false".into()),
            bin: "/Applications/My Tools/herdr".into(),
        };
        let cmd = backend
            .shell_close_window_by_id_guard_cmd("w1;false")
            .unwrap();
        assert_eq!(
            shlex::split(&cmd).unwrap(),
            vec![
                "env",
                "HERDR_SOCKET_PATH=/tmp/my socket;false",
                "/Applications/My Tools/herdr",
                "workspace",
                "close",
                "w1;false"
            ]
        );
    }

    #[test]
    fn missing_socket_does_not_fall_back_to_default_session() {
        let backend = HerdrBackend {
            socket: None,
            bin: "herdr".into(),
        };
        assert!(backend.resolve_instance_id().is_err());
        assert!(backend.is_running().is_err());
    }
}
