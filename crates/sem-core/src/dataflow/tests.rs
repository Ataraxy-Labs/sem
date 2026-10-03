//! Data-flow tests over small synthetic repos. Each asserts the exact flows
//! (source class -> sink class between named functions), including that
//! a source and a sink with no data dependency produce none, and that data
//! handed to unresolved code is reported as an escape, not dropped.

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::Value;

use super::models::Models;
use crate::parser::graph::EntityGraph;
use crate::parser::plugins::create_default_registry;

fn run(files: &[(&str, &str)]) -> Value {
    let dir = tempfile::tempdir().unwrap();
    for (p, src) in files {
        let path = dir.path().join(p);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, src).unwrap();
    }
    let paths: Vec<String> = files.iter().map(|(p, _)| p.to_string()).collect();
    let registry = create_default_registry();
    let (_, entities) = EntityGraph::build(dir.path(), &paths, &registry);
    let known: BTreeSet<String> = paths.iter().cloned().collect();
    let resolve = move |from: &str, spec: &str| -> Option<String> {
        if !spec.starts_with('.') {
            return None;
        }
        let dir = Path::new(from).parent().unwrap_or(Path::new(""));
        let joined = dir.join(spec);
        let mut norm: Vec<String> = Vec::new();
        for c in joined.components() {
            match c.as_os_str().to_str().unwrap() {
                "." => {}
                ".." => {
                    norm.pop();
                }
                s => norm.push(s.to_string()),
            }
        }
        let base = norm.join("/");
        ["", ".ts", ".js", "/index.ts"].iter().map(|e| format!("{base}{e}")).find(|c| known.contains(c))
    };
    let a = super::analyze(dir.path(), &paths, &entities, &resolve, &Models::builtin());
    a.to_json()
}

/// `class entity -> class entity` for every flow.
fn flows(v: &Value, key: &str) -> BTreeSet<String> {
    v[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            format!(
                "{} {} -> {} {}",
                f["source"]["class"].as_str().unwrap_or("?"),
                f["source"]["entity"].as_str().unwrap_or("?"),
                f["sink"]["class"].as_str().unwrap_or("?"),
                f["sink"]["entity"].as_str().unwrap_or("?")
            )
        })
        .collect()
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn entity<'v>(v: &'v Value, name: &str) -> &'v Value {
    v["entities"].as_array().unwrap().iter().find(|e| e["name"] == name).unwrap_or_else(|| panic!("no entity {name}: {v:#}"))
}

#[test]
fn python_env_to_exec_in_one_function() {
    let v = run(&[(
        "app.py",
        "import os\nimport subprocess\n\ndef deploy():\n    target = os.environ.get('TARGET')\n    cmd = 'ship ' + target\n    subprocess.run(cmd, shell=True)\n",
    )]);
    assert_eq!(flows(&v, "flows"), set(&["env deploy -> exec deploy"]), "{v:#}");
}

#[test]
fn python_no_data_dependency_no_flow() {
    let v = run(&[(
        "app.py",
        "import os\nimport subprocess\n\ndef deploy():\n    target = os.environ.get('TARGET')\n    subprocess.run(['ls', '-l'])\n    return target\n",
    )]);
    assert!(flows(&v, "flows").is_empty(), "{v:#}");
    // but the facts still say what it reads and runs
    let e = entity(&v, "deploy");
    assert!(e["reads"].as_array().unwrap().contains(&Value::from("env")));
    assert!(e["writes"].as_array().unwrap().contains(&Value::from("exec")));
}

#[test]
fn python_interprocedural_request_to_db() {
    let v = run(&[
        (
            "db.py",
            "import sqlite3\n\ndef find(name):\n    conn = sqlite3.connect('app.db')\n    cur = conn.cursor()\n    cur.execute(\"select * from u where n = '\" + name + \"'\")\n    return cur.fetchall()\n",
        ),
        (
            "web.py",
            "from flask import request\nfrom db import find\n\ndef lookup():\n    who = request.args.get('who')\n    rows = find(who)\n    return rows\n",
        ),
    ]);
    assert_eq!(flows(&v, "flows"), set(&["http-input lookup -> db find"]), "{v:#}");
    let f = &v["flows"][0];
    let path: Vec<&str> = f["path"].as_array().unwrap().iter().map(|s| s.as_str().unwrap()).collect();
    assert!(path.iter().any(|s| s.contains("calls find")), "{path:?}");
    assert_eq!(f["interprocedural"], true);
}

#[test]
fn python_return_value_flows_up_to_caller() {
    let v = run(&[(
        "tool.py",
        "import os\nimport subprocess\n\ndef get_cmd():\n    return os.getenv('CMD')\n\ndef main():\n    subprocess.call(get_cmd())\n",
    )]);
    assert_eq!(flows(&v, "flows"), set(&["env get_cmd -> exec main"]), "{v:#}");
    let path = v["flows"][0]["path"].to_string();
    assert!(path.contains("returned from get_cmd"), "{path}");
}

#[test]
fn python_module_state_carries_a_source() {
    let v = run(&[(
        "cfg.py",
        "import os\nimport logging\n\nSECRET = os.getenv('SECRET')\nlog = logging.getLogger(__name__)\n\ndef report():\n    log.info('secret is %s', SECRET)\n",
    )]);
    assert_eq!(flows(&v, "flows"), set(&["env <module> -> log report"]), "{v:#}");
    assert!(v["flows"][0]["throughState"].as_str().unwrap().ends_with("cfg.py:SECRET"));
    let e = entity(&v, "report");
    assert!(e["reads"].as_array().unwrap().contains(&Value::from("global:cfg.py:SECRET")), "{e:#}");
}

#[test]
fn python_object_fields_are_one_location() {
    let v = run(&[(
        "svc.py",
        "import os\n\nclass Client:\n    def __init__(self):\n        self.token = os.environ['TOKEN']\n        self.n = 0\n\n    def debug(self):\n        print(self.n)\n",
    )]);
    // field-insensitive: writing token taints the object, so reading n does too
    assert_eq!(flows(&v, "flows"), set(&["env Client.__init__ -> log Client.debug"]), "{v:#}");
    let e = entity(&v, "Client.__init__");
    assert!(e["writes"].as_array().unwrap().contains(&Value::from("field:Client.token")), "{e:#}");
}

#[test]
fn python_unresolved_callee_is_an_escape_not_silence() {
    let v = run(&[(
        "job.py",
        "import os\n\ndef run(worker):\n    secret = os.environ['KEY']\n    worker.process(secret)\n",
    )]);
    assert!(flows(&v, "flows").is_empty());
    let esc = v["escapes"].as_array().unwrap();
    assert_eq!(esc.len(), 1, "{v:#}");
    assert_eq!(esc[0]["source"]["class"], "env");
    assert!(v["coverage"]["unknown"].as_u64().unwrap() >= 1);
}

#[test]
fn python_name_only_sink_hint_is_labeled() {
    let v = run(&[(
        "q.py",
        "from flask import request\n\ndef search(cur):\n    cur.execute('select ' + request.args['q'])\n",
    )]);
    assert!(flows(&v, "flows").is_empty(), "{v:#}");
    assert_eq!(flows(&v, "possibleFlows"), set(&["http-input search -> db search"]), "{v:#}");
    assert_eq!(v["possibleFlows"][0]["sink"]["precision"], "name-only");
}

#[test]
fn python_call_of_a_base_method_reaches_overrides() {
    let v = run(&[(
        "jobs.py",
        "import os\nimport subprocess\n\nclass Step:\n    def run(self, arg):\n        return arg\n\nclass Shell(Step):\n    def run(self, arg):\n        subprocess.run(arg, shell=True)\n\ndef go(step: Step):\n    step.run(os.environ['CMD'])\n",
    )]);
    // `step` is declared a Step, but a Shell may be passed: its override runs
    assert_eq!(flows(&v, "flows"), set(&["env go -> exec Shell.run"]), "{v:#}");
}

#[test]
fn python_sanitizer_stops_a_flow() {
    let v = run(&[(
        "n.py",
        "import os\n\ndef show():\n    n = int(os.environ['N'])\n    print(n)\n",
    )]);
    assert!(flows(&v, "flows").is_empty(), "{v:#}");
}

#[test]
fn python_dynamic_features_are_unknown() {
    let v = run(&[(
        "d.py",
        "def call(obj, name, x):\n    fn = getattr(obj, name)\n    return fn(x)\n",
    )]);
    let e = entity(&v, "call");
    assert!(!e["dynamic"].as_array().unwrap().is_empty(), "{e:#}");
    assert!(v["coverage"]["dynamicMarkers"].as_u64().unwrap() >= 1);
}

#[test]
fn ts_express_handler_to_child_process() {
    let v = run(&[(
        "src/server.ts",
        "import express from 'express';\nimport { exec } from 'child_process';\n\nconst app = express();\napp.get('/run', (req, res) => {\n  const cmd = req.query.cmd;\n  exec(`sh -c ${cmd}`);\n  res.send('ok');\n});\n",
    )]);
    // `res.send('ok')` sends a constant: no flow into the response
    assert_eq!(flows(&v, "flows"), set(&["http-input <module> -> exec <module>"]), "{v:#}");
}

#[test]
fn ts_cross_file_import_call() {
    let v = run(&[
        ("src/util.ts", "import { execSync } from 'node:child_process';\n\nexport function run(c: string) {\n  return execSync(c);\n}\n"),
        ("src/main.ts", "import { run } from './util';\n\nexport function start() {\n  run(process.env.BOOT_CMD || 'true');\n}\n"),
    ]);
    assert_eq!(flows(&v, "flows"), set(&["env start -> exec run"]), "{v:#}");
}

#[test]
fn ts_untyped_member_call_is_unknown() {
    let v = run(&[("src/a.ts", "export function f(svc: any) {\n  svc.handle(process.env.X);\n}\n")]);
    assert!(flows(&v, "flows").is_empty());
    assert_eq!(v["escapes"].as_array().unwrap().len(), 1, "{v:#}");
}

#[test]
fn go_request_to_sql_and_env_to_exec() {
    let v = run(&[(
        "main.go",
        "package main\n\nimport (\n\t\"database/sql\"\n\t\"net/http\"\n\t\"os\"\n\t\"os/exec\"\n)\n\nfunc handler(db *sql.DB, w http.ResponseWriter, r *http.Request) {\n\tq := r.URL.Query().Get(\"q\")\n\tdb.Query(\"select \" + q)\n}\n\nfunc boot() {\n\tbin := os.Getenv(\"BIN\")\n\texec.Command(bin).Run()\n}\n",
    )]);
    assert_eq!(flows(&v, "flows"), set(&["http-input handler -> db handler", "env boot -> exec boot"]), "{v:#}");
}

#[test]
fn go_error_results_carry_no_data() {
    let v = run(&[(
        "main.go",
        "package main\n\nimport (\n\t\"log\"\n\t\"os\"\n)\n\nfunc load(p string) []byte {\n\tdata, err := os.ReadFile(p)\n\tif err != nil {\n\t\tlog.Printf(\"read failed: %v\", err)\n\t}\n\tlog.Println(len(data))\n\treturn data\n}\n",
    )]);
    // `err` is a status; `len(data)` is sanitized: no file-read -> log
    assert!(flows(&v, "flows").is_empty(), "{v:#}");
}

#[test]
fn rust_env_to_command_and_log() {
    let v = run(&[(
        "src/main.rs",
        "use std::process::Command;\n\nfn tool() -> String {\n    std::env::var(\"TOOL\").unwrap()\n}\n\nfn main() {\n    let t = tool();\n    println!(\"running {}\", t);\n    Command::new(t).status().unwrap();\n}\n",
    )]);
    assert_eq!(flows(&v, "flows"), set(&["env tool -> exec main", "env tool -> log main"]), "{v:#}");
}

#[test]
fn complexity_counts_decisions_and_nesting() {
    let v = run(&[(
        "c.py",
        "def f(xs, a, b):\n    for x in xs:\n        if x and a or b:\n            return 1\n        elif x:\n            return 2\n        else:\n            return 3\n    return 0\n",
    )]);
    let e = entity(&v, "f");
    // decisions: for, if, elif, `and`, `or` -> 5 + 1
    assert_eq!(e["cyclomatic"], 6, "{e:#}");
    // for +1; if +2 (nested); elif +1; else +1; `and`/`or` runs +2 -> 7
    assert_eq!(e["cognitive"], 7, "{e:#}");
}

#[test]
fn python_model_beats_a_vendored_library_body() {
    // The library's own source is in the input (vendored, or dependency
    // sources): `render_template_string` resolves into its body, where no model
    // applies. The model still names the sink.
    let v = run(&[
        ("flask/__init__.py", "request = None\n\ndef render_template_string(source, **context):\n    return source\n"),
        (
            "app.py",
            "from flask import request, render_template_string\n\ndef page():\n    return render_template_string(request.args.get('t'))\n",
        ),
    ]);
    assert_eq!(flows(&v, "flows"), set(&["http-input page -> template page"]), "{v:#}");
}

// ---- entry points agent PRs add: MCP tools, tRPC, fasthttp, routes ----------

#[test]
fn python_fastmcp_tool_arguments_are_input() {
    let v = run(&[
        (
            "notes/store.py",
            "import os\n\nBASE = '/srv/notes'\n\ndef load(name):\n    with open(os.path.join(BASE, name)) as f:\n        return f.read()\n",
        ),
        (
            "notes/server.py",
            "import subprocess\nfrom mcp.server.fastmcp import FastMCP, Context\nfrom notes.store import load\n\nmcp = FastMCP('notes')\n\n@mcp.tool()\ndef read_note(name: str) -> str:\n    return load(name)\n\n@mcp.tool()\ndef status(ctx: Context) -> str:\n    subprocess.run(ctx.request_id, shell=True)\n    return 'ok'\n\ndef register(app: FastMCP):\n    @app.tool()\n    def shell(cmd: str) -> str:\n        subprocess.run(cmd, shell=True)\n        return 'done'\n",
        ),
    ]);
    // `ctx: Context` is injected by the framework, not a tool argument
    assert_eq!(
        flows(&v, "flows"),
        set(&["tool-input read_note -> file-path load", "tool-input register -> exec register"]),
        "{v:#}"
    );
}

#[test]
fn python_flask_route_and_django_urlconf_parameters_are_input() {
    let v = run(&[
        (
            "site/web.py",
            "from flask import Flask\n\napp = Flask(__name__)\n\n@app.route('/files/<name>')\ndef show(name):\n    return open(name).read()\n\ndef helper(name):\n    return open(name).read()\n",
        ),
        ("shop/__init__.py", ""),
        ("shop/views.py", "import os\n\ndef detail(request, slug):\n    os.system('render ' + slug)\n\ndef unrouted(request, slug):\n    os.system('render ' + slug)\n"),
        ("shop/urls.py", "from django.urls import path\nfrom shop import views\n\nurlpatterns = [path('d/<slug>/', views.detail)]\n"),
    ]);
    // `helper` and `unrouted` are not registered: their parameters are not input
    assert_eq!(flows(&v, "flows"), set(&["http-input show -> file-path show", "http-input detail -> exec detail"]), "{v:#}");
}

#[test]
fn go_fasthttp_request_and_mcp_tool_request_are_input() {
    let v = run(&[(
        "main.go",
        "package main\n\nimport (\n\t\"context\"\n\t\"os\"\n\t\"path/filepath\"\n\n\t\"github.com/mark3labs/mcp-go/mcp\"\n\t\"github.com/mark3labs/mcp-go/server\"\n\t\"github.com/valyala/fasthttp\"\n)\n\nfunc handle(ctx *fasthttp.RequestCtx) {\n\tid := string(ctx.Request.Header.Peek(\"X-Id\"))\n\tos.ReadFile(filepath.Join(\"threads\", id+\".json\"))\n}\n\nfunc register(s *server.MCPServer, t mcp.Tool) {\n\ts.AddTool(t, func(ctx context.Context, req mcp.CallToolRequest) (*mcp.CallToolResult, error) {\n\t\tp := req.GetString(\"path\", \"\")\n\t\tos.ReadFile(p)\n\t\treturn nil, nil\n\t})\n}\n",
    )]);
    assert_eq!(flows(&v, "flows"), set(&["http-input handle -> file-path handle", "tool-input register -> file-path register"]), "{v:#}");
}

#[test]
fn rust_rmcp_parameters_and_websocket_are_input() {
    let v = run(&[(
        "src/lib.rs",
        "use rmcp::handler::server::tool::Parameters;\n\npub struct Files;\n\npub struct ReadArgs {\n    pub path: String,\n}\n\nimpl Files {\n    pub fn read(&self, Parameters(args): Parameters<ReadArgs>) -> String {\n        std::fs::read_to_string(&args.path).unwrap()\n    }\n}\n\npub async fn on_socket(socket: axum::extract::ws::WebSocket) {\n    let msg = socket.recv().await;\n    std::fs::remove_file(msg).unwrap();\n}\n",
    )]);
    let f = flows(&v, "flows");
    assert!(f.contains("tool-input Files.read -> file-path Files.read"), "{v:#}");
    assert!(f.contains("http-input on_socket -> file-path on_socket"), "{v:#}");
}

#[test]
fn ts_trpc_input_remix_loader_and_next_route_are_input() {
    let v = run(&[
        (
            "src/server/routers/box.ts",
            "import { z } from 'zod';\nimport { router, publicProcedure } from '../trpc';\n\nasync function ping(id: string) {\n  return fetch(`https://${id}.example.com/health`);\n}\n\nexport const boxRouter = router({\n  start: publicProcedure\n    .input(z.object({ id: z.string() }))\n    .mutation(async ({ ctx, input }) => {\n      return ping(input.id);\n    }),\n  list: publicProcedure.query(async ({ ctx }) => {\n    return fetch(ctx.listUrl);\n  }),\n});\n",
        ),
        (
            "app/routes/go.tsx",
            "import type { LoaderFunctionArgs } from '@remix-run/node';\n\nexport async function loader({ request }: LoaderFunctionArgs) {\n  const next = new URL(request.url).searchParams.get('next');\n  return fetch(next);\n}\n",
        ),
        (
            "app/api/run/route.ts",
            "import { execSync } from 'child_process';\n\nexport async function POST(req: Request) {\n  const body = await req.text();\n  execSync(body);\n}\n",
        ),
    ]);
    // `list` has no `.input(..)`: its `ctx` is not request input
    assert_eq!(
        flows(&v, "flows"),
        set(&["http-input <module> -> net-send ping", "http-input loader -> net-send loader", "http-input POST -> exec POST"]),
        "{v:#}"
    );
}

#[test]
fn minified_bundles_are_not_analyzed() {
    let long = format!("var a=process.env.X;require('child_process').exec(a);{}\n", "var b=1;".repeat(200));
    let v = run(&[("static/bundle.js", long.as_str()), ("static/app.min.js", "var a=process.env.X;require('child_process').exec(a);\n")]);
    assert_eq!(v["coverage"]["files"], 0, "{v:#}");
}

#[test]
fn python_typer_and_click_command_parameters_are_cli_input() {
    let v = run(&[
        (
            "tool/store.py",
            "import os\n\nclass Store:\n    def __init__(self, base):\n        self.base = base\n\n    def read(self, name):\n        with open(os.path.join(self.base, name)) as f:\n            return f.read()\n",
        ),
        (
            "tool/cli.py",
            "import subprocess\nimport typer\nimport click\nfrom tool.store import Store\n\napp = typer.Typer()\n\n@app.command()\ndef show(name: str, ctx: typer.Context):\n    store = Store('/srv')\n    print(store.read(name=name))\n    subprocess.run(ctx.info_name, shell=True)\n\ndef helper(name: str):\n    return open(name).read()\n\n@click.group()\ndef cli():\n    pass\n\n@cli.command()\n@click.argument('cmd')\ndef run(cmd):\n    subprocess.run(cmd, shell=True)\n\n@cli.command()\n@click.pass_context\ndef info(context, name):\n    subprocess.run(context.info_name, shell=True)\n    open(name)\n\ndef main(path: str):\n    return open(path).read()\n\nif __name__ == '__main__':\n    typer.run(main)\n",
        ),
    ]);
    // `helper` is not a command; `ctx: typer.Context` and a
    // `@click.pass_context` first parameter are injected
    let got = flows(&v, "flows");
    for want in ["cli-input show -> file-path Store.read", "cli-input run -> exec run", "cli-input main -> file-path main", "cli-input info -> file-path info"] {
        assert!(got.contains(want), "missing {want}: {got:?}");
    }
    assert!(!got.iter().any(|f| f.contains("helper") || f.starts_with("cli-input show -> exec") || f.starts_with("cli-input info -> exec")), "{got:?}");
}

#[test]
fn python_home_assistant_flow_steps_take_user_input() {
    let v = run(&[
        ("comp/__init__.py", "import requests\n\nasync def lookup(key):\n    return requests.get('https://api.example/' + key + '/x')\n"),
        (
            "comp/flow.py",
            "import os\nfrom homeassistant import config_entries\nfrom homeassistant.config_entries import ConfigFlow\nfrom . import lookup\n\nclass Options(config_entries.OptionsFlow):\n    async def async_step_init(self, user_input=None):\n        if user_input is not None:\n            self._key = user_input['key']\n        return await self.async_step_next()\n\n    async def async_step_next(self, user_input=None):\n        return await lookup(self._key)\n\n    def helper(self, cmd):\n        os.system(cmd)\n\nclass Base(ConfigFlow):\n    pass\n\nclass Flow(Base, domain='x'):\n    async def async_step_user(self, user_input=None):\n        os.system(user_input['cmd'])\n\nclass NotAFlow:\n    async def async_step_user(self, user_input=None):\n        os.system(user_input['cmd'])\n",
        ),
    ]);
    let got = flows(&v, "flows");
    // a step's input stored in a field reaches a later step's call; a flow
    // derived through a repo base class counts; `helper` and a class that is
    // not a flow do not
    for want in ["http-input Options.async_step_init -> net-send lookup", "http-input Flow.async_step_user -> exec Flow.async_step_user"] {
        assert!(got.contains(want), "missing {want}: {got:?}");
    }
    assert!(!got.iter().any(|f| f.contains("NotAFlow") || f.contains("helper")), "{got:?}");
}

#[test]
fn python_fastapi_route_parameters_are_input() {
    let v = run(&[
        ("svc/work.py", "import requests\n\ndef fetch(url):\n    return requests.get(url)\n"),
        (
            "svc/server.py",
            "import os\nimport subprocess\nfrom fastapi import FastAPI, APIRouter, BackgroundTasks, Depends\nfrom pydantic import BaseModel\nfrom svc.work import fetch\n\nrouter = APIRouter()\n\nclass Req(BaseModel):\n    url: str\n\nclass Query(BaseModel):\n    cmd: str\n\ndef get_cfg():\n    return 'x'\n\n@router.get('/f/{name}')\ndef read(name: str, tasks: BackgroundTasks, cfg: str = Depends(get_cfg), q: Query = Depends()):\n    os.system(tasks.name)\n    os.system(cfg)\n    subprocess.run(q.cmd)\n    return open(name).read()\n\nclass Server:\n    def __init__(self):\n        self.app = FastAPI()\n        self.routes()\n\n    def routes(self):\n        @self.app.post('/go')\n        async def go(req: Req):\n            return fetch(req.url)\n\ndef helper(name: str):\n    return open(name).read()\n",
        ),
    ]);
    let got = flows(&v, "flows");
    // a body model through a nested `@self.app.post`; `BackgroundTasks` and
    // `Depends(get_cfg)` are injected, a bare `Depends()` model is input
    for want in ["http-input read -> file-path read", "http-input Server.routes -> net-send fetch", "http-input read -> exec read"] {
        assert!(got.contains(want), "missing {want}: {got:?}");
    }
    assert!(!got.iter().any(|f| f.contains("helper")), "{got:?}");
    let execs: Vec<&Value> = v["flows"].as_array().unwrap().iter().filter(|f| f["sink"]["class"] == "exec").collect();
    assert!(execs.iter().all(|f| f["sink"]["line"] == 22), "{execs:#?}");
}

#[test]
fn python_typed_splat_parameters_receive_arguments() {
    let v = run(&[(
        "conn.py",
        "import os\nimport requests\nfrom typing import Any\n\nclass Conn:\n    @staticmethod\n    def fetch(data: str) -> str:\n        return requests.get(data.replace('ipfs://', 'https://')).text\n\n    def generate_text(self, prompt: str, system_prompt: str, model: str = None, **kwargs) -> str:\n        return self.fetch(system_prompt)\n\n    def perform_action(self, action_name: str, **kwargs: Any) -> Any:\n        return self.generate_text(**kwargs)\n\ndef main():\n    kw = {}\n    kw['system_prompt'] = os.environ['P']\n    Conn().perform_action('generate-text', **kw)\n",
    )]);
    // `**kwargs: Any` is a parameter: what is passed to it is forwarded
    assert!(flows(&v, "flows").contains("env main -> net-send Conn.fetch"), "{v:#}");
}

#[test]
fn python_thread_offload_runs_the_passed_function() {
    let v = run(&[
        ("work.py", "import os\n\nclass Agent:\n    def act(self, x):\n        os.system(x)\n\ndef job(path):\n    return open(path).read()\n"),
        (
            "st.py",
            "import asyncio\nimport os\nfrom work import Agent, job\n\nclass State:\n    def __init__(self):\n        self.agent = Agent()\n\n    async def go(self):\n        return await asyncio.to_thread(self.agent.act, os.getenv('Y'))\n\n    async def read(self):\n        loop = asyncio.get_running_loop()\n        return await loop.run_in_executor(None, job, os.getenv('P'))\n",
        ),
    ]);
    // the function handed to `asyncio.to_thread` / `run_in_executor` runs
    // with the arguments after it
    assert_eq!(flows(&v, "flows"), set(&["env State.go -> exec Agent.act", "env State.read -> file-path job"]), "{v:#}");
}

#[test]
fn python_none_placeholder_field_takes_its_later_type() {
    let v = run(&[
        ("agent.py", "import os\n\nclass Agent:\n    def perform_action(self, x):\n        os.system(x)\n"),
        (
            "cli.py",
            "import os\nfrom agent import Agent\n\nclass CLI:\n    def __init__(self):\n        self.agent = None\n\n    def load(self):\n        self.agent = Agent()\n\n    def act(self):\n        return self.agent.perform_action(os.environ['X'])\n",
        ),
        (
            "st.py",
            "import asyncio\nimport os\nfrom cli import CLI\n\nclass State:\n    def __init__(self):\n        self.cli = CLI()\n\n    async def go(self):\n        return await asyncio.to_thread(self.cli.agent.perform_action, os.getenv('Y'))\n",
        ),
    ]);
    // `self.agent = None` in `__init__` does not hide `self.agent = Agent()`;
    // `asyncio.to_thread` runs the method it is handed
    assert_eq!(flows(&v, "flows"), set(&["env CLI.act -> exec Agent.perform_action", "env State.go -> exec Agent.perform_action"]), "{v:#}");
}

#[test]
fn python_thread_offload_and_getattr_self_dispatch_are_followed() {
    let v = run(&[
        (
            "conn.py",
            "import requests\nimport subprocess\n\nclass Base:\n    def perform_action(self, action_name, **kwargs):\n        raise NotImplementedError\n\nclass Web(Base):\n    def __init__(self):\n        self.actions = {'fetch': self.fetch}\n\n    def perform_action(self, action_name, **kwargs):\n        method = getattr(self, action_name.replace('-', '_'))\n        return method(**kwargs)\n\n    def fetch(self, url):\n        return requests.get(url)\n\n    def shell(self, url):\n        subprocess.run(url, shell=True)\n\nclass Other:\n    def run(self, cmd):\n        subprocess.run(cmd, shell=True)\n\n    def call(self, obj, name, arg):\n        f = getattr(obj, name)\n        return f(arg)\n",
        ),
        (
            "manager.py",
            "from typing import Dict\nfrom conn import Base\n\nclass Manager:\n    def __init__(self):\n        self.connections: Dict[str, Base] = {}\n\n    def perform_action(self, name: str, action: str, params: list):\n        conn = self.connections[name]\n        kwargs = {}\n        for i, p in enumerate(['url']):\n            kwargs[p] = params[i]\n        return conn.perform_action(action, **kwargs)\n",
        ),
        (
            "server.py",
            "import asyncio\nfrom fastapi import FastAPI\nfrom manager import Manager\nfrom conn import Other\n\nclass Server:\n    def __init__(self):\n        self.app = FastAPI()\n        self.mgr = Manager()\n\n    def routes(self):\n        @self.app.post('/a')\n        async def act(body: dict):\n            return await asyncio.to_thread(self.mgr.perform_action, body['c'], action=body['a'], params=body['p'])\n\n        @self.app.post('/o')\n        async def other(q: str):\n            return Other().call(Other(), 'run', q)\n",
        ),
    ]);
    let got = flows(&v, "flows");
    // `asyncio.to_thread` runs `Manager.perform_action`, whose `connection`
    // may be a `Web`, whose `getattr(self, ..)` may select `fetch`
    assert!(got.contains("http-input Server.routes -> net-send Web.fetch"), "{got:?}");
    // `Web` holds `fetch` in a dispatch table, which narrows the candidates
    // (no flow into `Web.shell`); `getattr` on another object is not
    // resolved (no flow into `Other.run`)
    assert!(!got.iter().any(|f| f.contains("exec")), "{got:?}");
}
