-- blocks/wake_compile/init.lua — Wake Compile block (P0)
--
-- persona の wake 時 context を編纂して compiled prompt 1 本 + CompileTrace を
-- stdout に出力する。LLM 呼び出しは行わない (データ収集 + 整形のみ)。
--
-- 起動:
--   agent-block --script blocks/wake_compile/init.lua --prompt shi
--   WAKE_PERSONA=shi agent-block --script blocks/wake_compile/init.lua
--
-- 出力:
--   <compiled prompt text>
--   --- COMPILE TRACE ---
--   identity: included, 312 chars
--   attention: dropped, reason=budget_exceeded
--   ...
--
-- エラー耐性:
--   個別 MCP source が接続失敗しても block 全体は落とさず、
--   その section を "source unavailable" で trace 記録して続行する。

-- ============================================================
-- 設定: MCP server コマンド
-- ============================================================
-- TODO: agent-block の .mcp.json に persona 系 server を追記後、
--       コマンド名が確定したら定数を更新すること。
-- 実際に使う command は env var で override 可能にしている。

local CMD_PERSONA_PACK = std.env.get_or("WAKE_CMD_PERSONA_PACK", "persona-pack-mcp")
local CMD_PERSONA_WORK = std.env.get_or("WAKE_CMD_PERSONA_WORK", "persona-work-mcp")
local CMD_MINI_APP = std.env.get_or("WAKE_CMD_MINI_APP", "mini-app-mcp")
local CMD_PERSONA_JOURNAL = std.env.get_or("WAKE_CMD_PERSONA_JOURNAL", "persona-journal-mcp")

-- ============================================================
-- ユーティリティ
-- ============================================================

--- MCP call result から text 文字列を抽出する。
--- ok=false / content 空 / text ブロック無しの場合は nil を返す。
--- @param result table  mcp.call の戻り値
--- @return string|nil
local function extract_mcp_text(result)
    if not result or not result.ok then
        return nil
    end
    local blocks = result.content or {}
    local parts = {}
    for _, b in ipairs(blocks) do
        if b.type == "text" and b.text and b.text ~= "" then
            table.insert(parts, b.text)
        end
    end
    if #parts == 0 then
        return nil
    end
    return table.concat(parts, "\n")
end

--- 安全に MCP サーバーを接続する。失敗時は nil + エラー文字列を返す。
--- @param alias   string  接続ハンドル名 (mcp.call で使う名前)
--- @param command string  コマンド名
--- @param args    table|nil コマンド引数 (例: {"--mcp"})
--- @return boolean, string|nil  ok, error
local function safe_connect(alias, command, args)
    local ok, err = pcall(mcp.connect, alias, command, args or {})
    if not ok then
        return false, tostring(err)
    end
    return true, nil
end

--- 安全に MCP サーバーを切断する (エラーは log.warn のみ)。
--- @param alias string
local function safe_disconnect(alias)
    local ok, err = pcall(mcp.disconnect, alias)
    if not ok then
        log.warn("wake_compile: disconnect error for '" .. alias .. "': " .. tostring(err))
    end
end

--- deny list の pattern を文字列に適用し、match する行を除去する。
--- P0: 単純な substring match (行単位)。
--- @param text     string    入力テキスト
--- @param patterns table     { string, ... } の配列
--- @return string            フィルタ後テキスト
local function apply_deny(text, patterns)
    if not text or #patterns == 0 then
        return text
    end
    local lines = {}
    for line in (text .. "\n"):gmatch("([^\n]*)\n") do
        local blocked = false
        for _, pat in ipairs(patterns) do
            if pat ~= "" and line:find(pat, 1, true) then
                blocked = true
                break
            end
        end
        if not blocked then
            table.insert(lines, line)
        end
    end
    return table.concat(lines, "\n")
end

-- ============================================================
-- Source resolvers
-- ============================================================
-- 各 resolver は (persona_id) -> (text|nil, error_reason|nil) を返す。
-- 接続に失敗した場合も nil + reason を返し、block は続行する。

--- persona-pack から identity text を取得する。
--- @param persona_id string
--- @return string|nil, string|nil
local function resolve_identity(persona_id)
    local alias = "wake_pp_" .. persona_id
    local ok, err = safe_connect(alias, CMD_PERSONA_PACK)
    if not ok then
        return nil, "source unavailable: " .. (err or "connect failed")
    end

    local result = mcp.call(alias, "persona_render", {
        id = persona_id,
        format = "prompt",
    })
    safe_disconnect(alias)

    local text = extract_mcp_text(result)
    if not text then
        local reason = (result and result.error) or "empty response"
        return nil, "resolve failed: " .. tostring(reason)
    end
    return text, nil
end

--- persona-work から attention list を取得する。
--- @param persona_id string
--- @return string|nil, string|nil
local function resolve_attention(persona_id)
    local alias = "wake_pw_attn_" .. persona_id
    local ok, err = safe_connect(alias, CMD_PERSONA_WORK)
    if not ok then
        return nil, "source unavailable: " .. (err or "connect failed")
    end

    -- TODO: persona-work の実際の tool 名が確定したら更新する
    -- 現在の persona-work は mini-app table 経由 (attention_list) を想定
    local result = mcp.call(alias, "attention_list", { persona_id = persona_id })
    safe_disconnect(alias)

    local text = extract_mcp_text(result)
    if not text then
        local reason = (result and result.error) or "empty response"
        return nil, "resolve failed: " .. tostring(reason)
    end
    return text, nil
end

--- persona-work から schedule due scan を取得する。
--- @param persona_id string
--- @return string|nil, string|nil
local function resolve_schedule(persona_id)
    local alias = "wake_pw_sched_" .. persona_id
    local ok, err = safe_connect(alias, CMD_PERSONA_WORK)
    if not ok then
        return nil, "source unavailable: " .. (err or "connect failed")
    end

    local result = mcp.call(alias, "schedule_due_scan", { owner = persona_id })
    safe_disconnect(alias)

    local text = extract_mcp_text(result)
    if not text then
        local reason = (result and result.error) or "empty response"
        return nil, "resolve failed: " .. tostring(reason)
    end
    return text, nil
end

--- mini-app の mailbox table から unread items を取得する。
--- @param persona_id string
--- @return string|nil, string|nil
local function resolve_mailbox(persona_id)
    local alias = "wake_ma_mb_" .. persona_id
    -- mini-app-mcp は --mcp flag 付きで MCP server mode になる
    local ok, err = safe_connect(alias, CMD_MINI_APP, { "--mcp" })
    if not ok then
        return nil, "source unavailable: " .. (err or "connect failed")
    end

    -- mailbox は共有 table。to = 自分 or "*" (broadcast) を server filter で絞り、
    -- unread (read_by に自分が居ない) は client 側で判定する
    local result = mcp.call(alias, "list", {
        table = "mailbox",
        filter = {
            type = "or",
            filters = {
                { type = "eq", field = "to", value = persona_id },
                { type = "eq", field = "to", value = "*" },
            },
        },
        limit = 50,
    })
    safe_disconnect(alias)

    if not result or not result.ok then
        local reason = (result and result.error) or "call failed"
        return nil, "resolve failed: " .. tostring(reason)
    end

    -- list 結果を plain text に整形する
    local content_raw = result.content or {}
    local raw_text = nil
    for _, b in ipairs(content_raw) do
        if b.type == "text" and b.text then
            raw_text = b.text
            break
        end
    end

    if not raw_text or raw_text == "" then
        return nil, "empty mailbox"
    end

    -- JSON 配列をデコードして subject / body の先頭部を並べる
    local items_ok, items = pcall(std.json.decode, raw_text)
    if not items_ok or type(items) ~= "table" then
        -- デコード失敗時は raw text を使う
        return raw_text, nil
    end

    local lines = {}
    for _, row in ipairs(items) do
        local d = row.data or {}
        -- unread 判定: read_by 配列に自分が居なければ未読
        local already_read = false
        for _, reader in ipairs(d.read_by or {}) do
            if reader == persona_id then
                already_read = true
                break
            end
        end
        if not already_read then
            local subject = d.subject or "(件名なし)"
            local from = d.from or "?"
            table.insert(lines, "* [" .. from .. "] " .. subject)
        end
    end

    if #lines == 0 then
        return nil, "no unread mail"
    end
    return table.concat(lines, "\n"), nil
end

--- persona-journal から最新 journal を取得する。
--- @param persona_id string
--- @return string|nil, string|nil
local function resolve_journal(persona_id)
    local alias = "wake_pj_" .. persona_id
    local ok, err = safe_connect(alias, CMD_PERSONA_JOURNAL)
    if not ok then
        return nil, "source unavailable: " .. (err or "connect failed")
    end

    -- wake 用は states (現況) + emo (直近の気分) の最新 1 件ずつに絞る
    -- (kind="all" は memories 等も含み 3000+ chars で budget を確実に超える)
    local result = mcp.call(alias, "journal_query_latest", {
        persona = persona_id,
        kind = "states,emo",
        count = 1,
    })
    safe_disconnect(alias)

    local text = extract_mcp_text(result)
    if not text then
        local reason = (result and result.error) or "empty response"
        return nil, "resolve failed: " .. tostring(reason)
    end
    return text, nil
end

--- persona-work から product awareness list を取得する。
--- @param persona_id string
--- @return string|nil, string|nil
local function resolve_awareness(persona_id)
    local alias = "wake_pw_aware_" .. persona_id
    local ok, err = safe_connect(alias, CMD_PERSONA_WORK)
    if not ok then
        return nil, "source unavailable: " .. (err or "connect failed")
    end

    -- NOTE: awareness_list は persona-work HEAD (migration 006) で追加済だが、
    -- install 済 binary (v0.1.0 skeleton) には未搭載。binary 更新までは
    -- "tool not found" で graceful degrade する (P0 想定内)
    local result = mcp.call(alias, "awareness_list", { persona_id = persona_id })
    safe_disconnect(alias)

    local text = extract_mcp_text(result)
    if not text then
        local reason = (result and result.error) or "empty response"
        return nil, "resolve failed: " .. tostring(reason)
    end
    return text, nil
end

--- deny list を persona-work から取得する。
--- 取得失敗時は空配列を返す (deny filter が機能しないだけで続行)。
--- @param persona_id string
--- @return table  { string, ... }
local function fetch_deny_patterns(persona_id)
    local alias = "wake_pw_deny_" .. persona_id
    local ok, _ = safe_connect(alias, CMD_PERSONA_WORK)
    if not ok then
        log.warn("wake_compile: deny list unavailable, skip deny filter")
        return {}
    end

    local result = mcp.call(alias, "deny_list", { persona_id = persona_id })
    safe_disconnect(alias)

    if not result or not result.ok then
        log.warn("wake_compile: deny_list call failed, skip deny filter")
        return {}
    end

    -- text 抽出して行分割
    local text = extract_mcp_text(result)
    if not text or text == "" then
        return {}
    end

    local patterns = {}
    for line in (text .. "\n"):gmatch("([^\n]+)\n") do
        local trimmed = line:match("^%s*(.-)%s*$")
        if trimmed ~= "" then
            table.insert(patterns, trimmed)
        end
    end
    return patterns
end

-- ============================================================
-- Source resolver マップ
-- ============================================================

local RESOLVERS = {
    identity = resolve_identity,
    attention = resolve_attention,
    schedule = resolve_schedule,
    mailbox = resolve_mailbox,
    journal = resolve_journal,
    awareness = resolve_awareness,
}

-- ============================================================
-- Compile 本体
-- ============================================================

--- policy table と persona_id を受け取り、compiled prompt + trace を返す。
--- @param policy     table   policy/shi.lua が返す table
--- @param persona_id string
--- @return string  compiled prompt
--- @return table   trace: { { feed, status, chars, reason }, ... }
local function compile(policy, persona_id)
    -- 1. deny patterns を先に取得 (全 section 後処理に必要)
    local deny_patterns = {}
    if policy.deny then
        deny_patterns = fetch_deny_patterns(persona_id)
        log.info("wake_compile: deny patterns loaded: " .. #deny_patterns)
    end

    -- 2. 各 section を priority 昇順でソートしてから resolve
    local sections = {}
    for _, s in ipairs(policy.sections) do
        table.insert(sections, s)
    end
    table.sort(sections, function(a, b)
        return (a.priority or 99) < (b.priority or 99)
    end)

    local prompt_parts = {}
    local trace = {}
    local used_chars = 0
    local budget = policy.budget or 4000

    for _, sec in ipairs(sections) do
        local feed = sec.feed
        local max_ch = sec.max_chars or 500
        local resolver = RESOLVERS[feed]

        -- resolver が未定義の場合は skip
        if not resolver then
            table.insert(trace, {
                feed = feed,
                status = "skipped",
                chars = 0,
                reason = "no resolver defined for src=" .. (sec.src or "?"),
            })
            goto continue
        end

        -- 全体 budget を超えていたら以降を全部 drop
        if used_chars >= budget then
            table.insert(trace, {
                feed = feed,
                status = "dropped",
                chars = 0,
                reason = "global budget exhausted (" .. used_chars .. "/" .. budget .. ")",
            })
            goto continue
        end

        -- source を resolve
        local text, resolve_err = resolver(persona_id)

        if not text then
            -- source 取得失敗 → trace に記録して続行
            table.insert(trace, {
                feed = feed,
                status = "dropped",
                chars = 0,
                reason = resolve_err or "resolve returned nil",
            })
            goto continue
        end

        -- deny filter 適用
        text = apply_deny(text, deny_patterns)

        -- section budget チェック
        if #text > max_ch then
            -- 超過: draft §2 方針に従い drop + trace 記録 (truncate しない)
            table.insert(trace, {
                feed = feed,
                status = "dropped",
                chars = #text,
                reason = "section budget exceeded (" .. #text .. " > " .. max_ch .. ")",
            })
            goto continue
        end

        -- 全体 budget 残量チェック
        if used_chars + #text > budget then
            table.insert(trace, {
                feed = feed,
                status = "dropped",
                chars = #text,
                reason = "global budget would be exceeded (used="
                    .. used_chars
                    .. " + this="
                    .. #text
                    .. " > "
                    .. budget
                    .. ")",
            })
            goto continue
        end

        -- include
        table.insert(prompt_parts, "## " .. feed .. "\n" .. text)
        used_chars = used_chars + #text
        table.insert(trace, {
            feed = feed,
            status = "included",
            chars = #text,
            reason = nil,
        })

        ::continue::
    end

    local compiled = table.concat(prompt_parts, "\n\n")
    return compiled, trace
end

-- ============================================================
-- エントリポイント
-- ============================================================

-- persona_id の決定: _PROMPT → env WAKE_PERSONA → default "shi"
local persona_id = (_PROMPT and _PROMPT ~= "" and _PROMPT) or std.env.get_or("WAKE_PERSONA", "shi")
-- _PROMPT は "shi\n" のように trailing newline を含む場合があるため trim する
persona_id = persona_id:match("^%s*(.-)%s*$")

log.info("wake_compile: persona_id=" .. persona_id)

-- policy を load (persona 別 module を動的 require)
-- P0 は shi のみ実装済み。他 persona は policy が無く fallback エラーになる
local policy_module = "wake_compile.policy." .. persona_id
local policy_ok, policy = pcall(require, policy_module)
if not policy_ok then
    -- policy が無い persona は空 policy で続行 (section 全 skip → 空 prompt)
    log.warn("wake_compile: no policy for persona '" .. persona_id .. "', using empty policy")
    policy = { budget = 4000, sections = {}, deny = nil }
end

-- compile 実行
local compiled, trace = compile(policy, persona_id)

-- ============================================================
-- 出力
-- ============================================================

print(compiled)
print("")
print("--- COMPILE TRACE ---")
for _, t in ipairs(trace) do
    local line = t.feed .. ": " .. t.status
    if t.chars and t.chars > 0 then
        line = line .. ", " .. t.chars .. " chars"
    end
    if t.reason then
        line = line .. ", reason=" .. t.reason
    end
    print(line)
end
