# claude-hooks

Tarefas de monitoramento que, ao disparar, abrem uma sessão headless do Claude Code (`claude -p`) para analisar o que a tarefa pede. Cada execução fica registrada com prompt, relatório, custo e `session_id` para continuar a análise com `claude --resume`.

## Gatilhos

| Tipo | Dispara quando | Dados enviados ao Claude |
|---|---|---|
| `cron` | horário da expressão cron (5 campos; 6 com segundos) | horário do agendamento |
| `command` | um comando de checagem, rodado a cada `interval`, satisfaz `when` | comando, exit code e saída (stdout + stderr) |
| `webhook` | `POST /hooks/<tarefa>` no daemon | content-type, query string e corpo |

Condições de `when` para `command`:

| `when` | Dispara se |
|---|---|
| `exit_nonzero` (padrão) | exit code ≠ 0 ou timeout |
| `exit_zero` | exit code = 0 |
| `output_matches` | a saída casa com `pattern` (regex) |
| `output_not_matches` | a saída não casa com `pattern` |
| `output_changed` | a saída difere da checagem anterior |
| `always` | toda checagem |

Cada tarefa roda no máximo uma sessão por vez. `cooldown` define o intervalo mínimo entre disparos; o padrão é 30m para `command` e 0 para os demais. `max_concurrent` no `config.toml` limita sessões simultâneas entre todas as tarefas.

## Instalação

```sh
cargo install --path .
claude-hooks init
```

No Windows sem Windows SDK, compile com o toolchain GNU: `cargo +stable-x86_64-pc-windows-gnu install --path .`.

Requer o `claude` no PATH e autenticado (`claude -p "oi"` precisa funcionar no mesmo usuário que roda o daemon).

## Uso

```sh
# checagem por comando
claude-hooks add disco \
  --command "ssh mono df -h / | tail -1" --every 5m \
  --when output_matches --pattern "9[0-9]%" --cooldown 2h \
  --prompt "O disco do mono passou de 90%. Descubra o que ocupa espaço e o que pode ser removido." \
  --cwd C:/Users/arthu/desktop/WMC --model sonnet --allow "Bash(ssh mono *)"

# agendada
claude-hooks add resumo --cron "0 8 * * MON-FRI" --prompt-file prompts/resumo.md

# webhook
claude-hooks add alerta --webhook --token segredo --prompt "Faça a triagem do alerta recebido."

# a partir de arquivo TOML (ver examples/)
claude-hooks import examples/pg-disco.toml

claude-hooks daemon            # agendador + servidor HTTP
```

Gerenciamento:

```sh
claude-hooks list
claude-hooks show <tarefa>
claude-hooks enable|disable <tarefa>
claude-hooks remove <tarefa>
claude-hooks check <tarefa>                 # roda só a checagem e mostra se dispararia
claude-hooks run <tarefa> --context "..."   # dispara o Claude agora
claude-hooks runs [tarefa]                  # histórico
claude-hooks result <tarefa> [run_id]       # relatório (padrão: o mais recente)
```

O daemon relê `tasks/` a cada 5 segundos; tarefas adicionadas ou editadas não exigem reinício.

Chamando um webhook:

```sh
curl -X POST http://127.0.0.1:7878/hooks/alerta \
  -H "Authorization: Bearer segredo" -H "Content-Type: application/json" \
  -d '{"alert": "CPU 99%", "host": "mono"}'
```

O token também é aceito em `X-Hook-Token` ou `?token=`. Respostas: `202` iniciado, `409` sessão anterior em execução, `429` em cooldown, `401` token inválido, `404` tarefa inexistente. `GET /tasks` lista as tarefas e `GET /health` responde `ok`.

## Formato da tarefa

`~/.claude-hooks/tasks/<nome>.toml`:

```toml
description = "Disco do Postgres no mono"
enabled = true
prompt = """
O que o Claude deve analisar quando disparar.
"""
cooldown = "2h"        # opcional
timeout = "15m"        # tempo máximo da sessão (padrão 20m)

[trigger]
type = "command"       # cron | command | webhook
command = "ssh mono df -h / | tail -1"
interval = "5m"
when = "output_matches"
pattern = "9[0-9]%"
timeout = "60s"        # tempo máximo da checagem (padrão 60s)
shell = ["bash", "-c"] # padrão: cmd /C no Windows, sh -c nos demais

[claude]
cwd = "C:/Users/arthu/desktop/WMC"  # CLAUDE.md, skills e .claude/settings.json do projeto valem
model = "sonnet"
permission_mode = "plan"            # default | acceptEdits | plan | bypassPermissions
allowed_tools = ["Bash(ssh mono *)", "Read"]
disallowed_tools = []
add_dirs = []
append_system_prompt = "..."
max_budget_usd = 2.0
effort = "high"
extra_args = []                     # repassados ao `claude`

[notify]
webhook = "https://..."             # POST JSON com o resultado
command = "..."                     # roda ao fim com CLAUDE_HOOKS_* no ambiente
on = "always"                       # always | success | failure
```

Durações usam o formato do `humantime` (`30s`, `5m`, `2h`, `1d`).

### Permissões

A sessão roda sem ninguém para aprovar ferramentas: o que não estiver liberado por `allowed_tools`, `permission_mode` ou pelo `settings.json` do projeto em `cwd` é negado. Libere o mínimo que a análise precisa, por exemplo `Bash(ssh mono *)` para inspeção remota. `bypassPermissions` libera tudo.

### Prompt enviado

O prompt da tarefa recebe um bloco com o nome da tarefa, horário, origem do disparo e os dados do evento (saída do comando ou corpo do webhook, cortados em 60 KB mantendo início e fim). O arquivo final fica em `runs/<tarefa>/<run_id>/prompt.md`.

### Notificação

Payload do `notify.webhook`:

```json
{
  "task": "disco", "run_id": "20260923-111530", "status": "ok",
  "event": {"source": "command", "summary": "pattern encontrado", "details": "..."},
  "result": "relatório em markdown", "error": null,
  "session_id": "7cbcf67d-...", "cost_usd": 0.02,
  "started_at": "...", "finished_at": "...", "run_dir": "..."
}
```

Variáveis do `notify.command`: `CLAUDE_HOOKS_TASK`, `CLAUDE_HOOKS_RUN_ID`, `CLAUDE_HOOKS_STATUS`, `CLAUDE_HOOKS_RUN_DIR`, `CLAUDE_HOOKS_RESULT_FILE`, `CLAUDE_HOOKS_SESSION_ID`. A sessão do Claude recebe `CLAUDE_HOOKS_TASK` e `CLAUDE_HOOKS_RUN_DIR`, úteis para hooks do próprio Claude Code.

## Diretórios

```
~/.claude-hooks/            (ou $CLAUDE_HOOKS_HOME)
├── config.toml             listen, claude_bin, max_concurrent
├── tasks/<nome>.toml
├── state/<nome>.json       último disparo e última saída da checagem
└── runs/<nome>/<run_id>/
    ├── meta.json           status, custo, session_id, evento
    ├── prompt.md
    ├── result.md
    ├── claude-stdout.json
    └── claude-stderr.log
```

`config.toml`:

```toml
listen = "127.0.0.1:7878"
claude_bin = "claude"
max_concurrent = 2
```

Nível de log via `CLAUDE_HOOKS_LOG` (ex.: `debug` mostra checagens que não dispararam).

## Rodando como serviço

Linux (systemd, `~/.config/systemd/user/claude-hooks.service`):

```ini
[Unit]
Description=claude-hooks

[Service]
ExecStart=%h/.cargo/bin/claude-hooks daemon
Restart=on-failure

[Install]
WantedBy=default.target
```

```sh
systemctl --user enable --now claude-hooks
loginctl enable-linger $USER
```

Windows (Agendador de Tarefas, no logon do usuário):

```powershell
$a = New-ScheduledTaskAction -Execute "$env:USERPROFILE\.cargo\bin\claude-hooks.exe" -Argument "daemon"
$t = New-ScheduledTaskTrigger -AtLogOn
Register-ScheduledTask -TaskName "claude-hooks" -Action $a -Trigger $t -Settings (New-ScheduledTaskSettingsSet -ExecutionTimeLimit 0)
```
