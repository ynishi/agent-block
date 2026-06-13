-- blocks/wake_compile/policy/shi.lua — shi の編纂 policy
--
-- wake_compile/init.lua が require("wake_compile.policy.shi") で読み込む。
-- 各 section は source 取得方法 + budget (max_chars) + 優先度を定義する。
-- Deny は横断 filter (全 section 出力に最後に適用)。
--
-- max_chars: P0 では token 概算を chars で代用 (1 token ≈ 1.5–2 chars の粗い見積)
-- budget 競合: priority 昇順 (小さい数が高優先) で配分し、超過 section は drop + trace

return {
    -- 全体上限 (chars)
    budget = 4000,

    -- section 一覧: 上から順に評価。priority は budget 競合時の残存判断に使う
    sections = {
        {
            feed = "identity",
            src = "pack", -- persona-pack: persona_render
            max_chars = 600,
            priority = 1,
        },
        {
            feed = "attention",
            src = "attention", -- persona-work: attention_list
            max_chars = 800,
            priority = 2,
            order = "priority", -- attention_list の sort 軸 (server 側で対応時)
        },
        {
            feed = "schedule",
            src = "schedule", -- persona-work: schedule_due_scan
            max_chars = 400,
            priority = 3,
        },
        {
            feed = "mailbox",
            src = "mailbox", -- mini-app: mailbox table, unread filter
            max_chars = 1200, -- shi 宛 + broadcast の未読が多い実態に合わせ拡張 (初版 300 で drop 実測)
            priority = 4,
            filter = "unread",
        },
        {
            feed = "journal",
            src = "journal", -- persona-journal: journal_query_latest (states + emo 最新 1 件ずつ)
            max_chars = 800, -- states+emo 2 件で実測 631 chars、600 では drop した
            priority = 5,
        },
        {
            feed = "awareness",
            src = "awareness", -- persona-work: awareness_list 相当
            max_chars = 500,
            priority = 6,
        },
    },

    -- deny: persona-work の deny_list を横断 filter として使う
    -- 文字列の単純 match (string.find) で全 section 出力に適用する
    deny = "deny", -- src key: deny_list を resolve する source 識別子
}
