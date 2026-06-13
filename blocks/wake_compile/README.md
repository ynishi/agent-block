# blocks/wake_compile

persona の wake 時 context を編纂して **compiled prompt 1 本 + CompileTrace** を stdout に出力する block。
LLM 呼び出しは行わない (データ収集 + 整形のみ)。

## 起動

```sh
# --prompt で persona_id を指定 (推奨)
agent-block --script blocks/wake_compile/init.lua --prompt shi

# env var でも指定可能
WAKE_PERSONA=shi agent-block --script blocks/wake_compile/init.lua
```

## 前提

- `agent-block` binary が PATH に存在すること
- 以下の MCP server が起動可能なこと (command は env var で override 可):

| server | 役割 | デフォルト command | env var |
|---|---|---|---|
| persona-pack | identity (persona_render) | `persona-pack-mcp` | `WAKE_CMD_PERSONA_PACK` |
| persona-work | attention / schedule / awareness / deny | `persona-work-mcp` | `WAKE_CMD_PERSONA_WORK` |
| mini-app | mailbox (unread) | `mini-app-mcp` | `WAKE_CMD_MINI_APP` |
| persona-journal | journal (latest) | `persona-journal-mcp` | `WAKE_CMD_PERSONA_JOURNAL` |

MCP server が接続できない場合、その section は `source unavailable` として CompileTrace に記録され、block 全体は続行する。

## 出力形式

```
## identity
<persona identity text>

## attention
<attention items>

--- COMPILE TRACE ---
identity: included, 312 chars
attention: included, 540 chars
schedule: dropped, reason=source unavailable: connect failed
mailbox: included, 180 chars
journal: dropped, reason=section budget exceeded (720 > 600)
awareness: dropped, reason=global budget exhausted (1032/4000)
```

## policy のカスタマイズ

`blocks/wake_compile/policy/shi.lua` を編集して budget / section 順序 / max_chars を調整する。
他 persona に対応するには `policy/<persona_id>.lua` を同形式で追加する。
