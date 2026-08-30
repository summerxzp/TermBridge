//! ansi_strip —— Terminal control sequence stripping（Phase 8）
//!
//! 剥离 PTY 输出中的终端控制序列，返回纯文本视图。
//!
//! ## 设计目标
//!
//! RingBuffer 永远保留 raw bytes（ADR-0012 契约 ③ Cursor）。
//! `read_output(strip_ansi=true)` 只改变返回给调用者的数据，不改 RingBuffer。
//!
//! ## 覆盖的序列类型
//!
//! | 类型 | 转义 | 示例 | 终止符 |
//! |------|------|------|--------|
//! | CSI  | `ESC [` | `\x1b[?2004h`（bracketed paste） | 0x40-0x7E（`@A-Z[\]^_`a-z{|}~`）|
//! | OSC  | `ESC ]` | `\x1b]7;file://...`（OSC 7 工作目录通知） | `BEL`(`\x07`) 或 `ST`(`ESC \`) |
//! | DCS  | `ESC P` | 设备控制字符串 | `ST`(`ESC \`) |
//! | APC  | `ESC _` | 应用程序命令 | `ST`(`ESC \`) |
//! | PM   | `ESC ^` | 私有消息 | `ST`(`ESC \`) |
//! | SOS  | `ESC X` | 字符串参数 | `ST`(`ESC \`) |
//! | 其他转义 | `ESC <i>*<f>` | `ESC c`(RIS)、`ESC ( B`(字符集)、`ESC % G`、`ESC # 8` | 终止字节 `0x30-0x7E`（中间字节 `0x20-0x2F` 任意个）|
//!
//! 参考：ECMA-48 / xterm ctlseqs
//!
//! ## 不剥离
//!
//! - `\n` / `\r` / `\t` 等可打印控制字符（保留语义）
//! - 其他非 ESC 开头的字节（原样保留）
//!
//! ## 跨页续接（strip_page）
//!
//! `strip_control_sequences` 是无状态纯函数：序列被页边界（`since_cursor`
//! 分页的 `max_bytes`）拦腰截断时，页尾的半个序列被丢弃、尾部泄漏成正文。
//! `strip_page(input, pending)` 把上一页遗留的尾部不完整序列拼接后剥离，
//! 返回文本 + 新的尾部遗留；`OutputEngine` 在顺序连续读时传递遗留字节实现
//! 跨页续接（非连续读退回无状态剥离，best-effort）。
//!
//! ## 扩展性
//!
//! 当前 `strip_control_sequences(&[u8]) -> Vec<u8>` 是无配置的纯函数。
//! 未来如需选择性剥离（如保留颜色、剥离 OSC），可引入 `StripMode` 枚举参数，
//! 不破坏现有调用方。

/// 剥离终端控制序列，返回纯文本视图。
///
/// 输入：PTY raw bytes（含 ANSI/OSC/DCS 等控制序列）
/// 输出：剥离控制序列后的纯文本字节
///
/// 不分配额外缓冲区以外的内存；输入 N 字节 → 输出 ≤ N 字节。
pub fn strip_control_sequences(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;

    while i < input.len() {
        let b = input[i];

        // ESC = 0x1B
        if b == 0x1B {
            // 需要至少 2 字节判断序列类型
            if i + 1 >= input.len() {
                // 孤立的 ESC 末尾：保留并结束
                out.push(b);
                break;
            }
            let next = input[i + 1];

            match next {
                // CSI: ESC [ ... <final byte 0x40-0x7E>
                b'[' => {
                    if let Some(consumed) = consume_csi(&input[i + 2..]) {
                        i += 2 + consumed;
                    } else {
                        // 不完整的 CSI（到末尾仍未找到 final byte）：跳过剩余
                        i = input.len();
                    }
                }
                // OSC: ESC ] ... <BEL 或 ST>
                b']' => {
                    i += 2 + consume_string_terminator(&input[i + 2..], b'\x07');
                }
                // DCS: ESC P ... <ST>
                b'P' => {
                    i += 2 + consume_string_terminator(&input[i + 2..], 0);
                }
                // APC: ESC _ ... <ST>
                b'_' => {
                    i += 2 + consume_string_terminator(&input[i + 2..], 0);
                }
                // PM: ESC ^ ... <ST>
                b'^' => {
                    i += 2 + consume_string_terminator(&input[i + 2..], 0);
                }
                // SOS: ESC X ... <ST>
                b'X' => {
                    i += 2 + consume_string_terminator(&input[i + 2..], 0);
                }
                // 其余转义序列（ECMA-48）：ESC + 任意个中间字节(0x20-0x2F) + 一个终止字节(0x30-0x7E)。
                // 常见：ESC c(RIS)、ESC 7/8(保存/恢复光标)、ESC ( B / ESC ) 0(字符集指定，
                // less/vim/man/ncurses 高频输出)、ESC # 8(DECALN)、ESC % G(UTF-8 模式)。
                // 修复 P1-11a：此前对未匹配字节一律只跳 2 字节，导致 3+ 字节序列的
                // 尾部字节（如 ESC ( B 的 'B'）泄漏为正文。
                _ => {
                    let mut j = i + 1;
                    // 中间字节（0 个或多个）：0x20-0x2F
                    while j < input.len() && (0x20..=0x2F).contains(&input[j]) {
                        j += 1;
                    }
                    if j < input.len() && (0x30..=0x7E).contains(&input[j]) {
                        // 完整序列：跳过 ESC + 中间字节 + 终止字节
                        i = j + 1;
                    } else if j >= input.len() {
                        // 序列在缓冲区末尾被截断（ESC + 中间字节后无终止字节）：跳过剩余
                        i = input.len();
                    } else {
                        // 畸形序列（中间字节后跟非终止字节）：只吞掉 ESC + 中间字节，
                        // 后续字节按正文处理（保守，不吞正文）
                        i = j;
                    }
                }
            }
        } else {
            // 普通字节：保留
            out.push(b);
            i += 1;
        }
    }

    out
}

/// 消费 CSI 序列的参数部分，返回消费的字节数（不含 `ESC [` 前缀）。
///
/// CSI 序列结构：`ESC [ <parameter bytes> <intermediate bytes> <final byte>`
/// - parameter bytes: 0x30-0x3F（`0-9:;<=>?`）
/// - intermediate bytes: 0x20-0x2F（空格到 `/`）
/// - final byte: 0x40-0x7E（`@` 到 `~`）
///
/// 返回 `Some(n)` 表示消费了 n 字节并找到 final byte；
/// 返回 `None` 表示到末尾仍未找到 final byte（不完整序列）。
fn consume_csi(rest: &[u8]) -> Option<usize> {
    let mut i = 0;
    // parameter bytes (0x30-0x3F)
    while i < rest.len() && (0x30..=0x3F).contains(&rest[i]) {
        i += 1;
    }
    // intermediate bytes (0x20-0x2F)
    while i < rest.len() && (0x20..=0x2F).contains(&rest[i]) {
        i += 1;
    }
    // final byte (0x40-0x7E)
    if i < rest.len() && (0x40..=0x7E).contains(&rest[i]) {
        i += 1;
        Some(i)
    } else {
        None
    }
}

/// 消费字符串型序列（OSC/DCS/APC/PM/SOS）的正文，返回消费的字节数
///（不含 `ESC ]/P/_/^/X` 前缀）。
///
/// 终止方式：
/// - `BEL`（`\x07`）：xterm OSC 常见终止
/// - `ST`（`ESC \`）：标准 String Terminator
///
/// `bel_terminator` 参数：
/// - `0x07`：OSC 用 BEL 终止
/// - `0`：其他类型（DCS/APC/PM/SOS）仅用 ST 终止
fn consume_string_terminator(rest: &[u8], bel_terminator: u8) -> usize {
    let mut i = 0;
    while i < rest.len() {
        // BEL 终止（仅 OSC）
        if bel_terminator != 0 && rest[i] == bel_terminator {
            return i + 1;
        }
        // ST 终止：ESC \
        if rest[i] == 0x1B && i + 1 < rest.len() && rest[i + 1] == b'\\' {
            return i + 2;
        }
        // 单独的 ESC 非完整 ST：保守起见视为序列结束（防止吞掉后续合法 ESC）
        // 但仅在 BEL 模式下才这样处理，避免 OSC 未终止吞掉后续数据
        if rest[i] == 0x1B && bel_terminator != 0 {
            return i;
        }
        i += 1;
    }
    // 到末尾仍未终止：消费全部剩余
    i
}

// ───────────────────────────────────────────────────────────────────────────
// 跨页续接剥离（修复 P1-11b）
// ───────────────────────────────────────────────────────────────────────────

/// strip_page 单次遗留字节的容量上限：超过则视为垃圾数据放弃续接
/// （防止未终止的 OSC 吞掉整页后把遗留缓冲撑到无限大）。
const MAX_PENDING_BYTES: usize = 8 * 1024;

/// `strip_page` 的返回结果。
pub struct StripPageResult {
    /// 本页剥离后的纯文本
    pub text: Vec<u8>,
    /// 输入尾部不完整的转义序列字节（应留待下一页拼接后重新剥离）。
    /// 序列完整（或不存在 ESC 序列）时为空。
    pub trailing: Vec<u8>,
}

/// 跨页剥离：把上一页遗留的不完整序列前缀（`pending`）拼接在本页开头后统一剥离。
///
/// 页边界会把 CSI/OSC 等序列拦腰截断：无状态剥离会静默丢弃页尾的半个序列，
/// 并把序列尾部（如 `m`、`2004l`）泄漏成正文。此函数返回剥离后的文本与新的
/// 尾部不完整序列；调用方（OutputEngine）在"顺序连续读"时把 `trailing` 传给
/// 下一页，即可正确续接跨页序列。
///
/// 非连续读（任意 cursor / tail / wait_for 扫描）应退回无状态的
/// `strip_control_sequences`（best-effort），不参与续接。
pub fn strip_page(input: &[u8], pending: &[u8]) -> StripPageResult {
    if pending.is_empty() {
        strip_page_inner(input)
    } else {
        let mut full = Vec::with_capacity(pending.len() + input.len());
        full.extend_from_slice(pending);
        full.extend_from_slice(input);
        strip_page_inner(&full)
    }
}

fn strip_page_inner(full: &[u8]) -> StripPageResult {
    match incomplete_trailing_start(full) {
        None => StripPageResult {
            text: strip_control_sequences(full),
            trailing: Vec::new(),
        },
        Some(start) => {
            let text = strip_control_sequences(&full[..start]);
            if full.len() - start > MAX_PENDING_BYTES {
                // 遗留超上限（如未终止的 OSC 吞掉了整页）：放弃续接，丢弃尾部。
                // 后续字节按正文处理（与无状态剥离对未终止 OSC 的兜底一致）。
                StripPageResult {
                    text,
                    trailing: Vec::new(),
                }
            } else {
                StripPageResult {
                    text,
                    trailing: full[start..].to_vec(),
                }
            }
        }
    }
}

/// 找出 input 末尾不完整转义序列的起点。
///
/// 返回 `Some(start)`：`input[start..]` 是被缓冲区末尾截断的序列前缀；
/// 返回 `None`：所有 ESC 序列均完整（或不存在 ESC）。
fn incomplete_trailing_start(input: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i < input.len() {
        if input[i] == 0x1B {
            match sequence_end(input, i) {
                Some(end) => i = end,
                None => return Some(i),
            }
        } else {
            i += 1;
        }
    }
    None
}

/// 计算从 `input[esc]` 的 ESC 开始的完整转义序列长度；序列不完整（被末尾截断）
/// 返回 `None`。分支与 `strip_control_sequences` 的消费逻辑一一对应，
/// 保证"完整前缀用纯函数剥离"与"整体剥离"结果一致。
fn sequence_end(input: &[u8], esc: usize) -> Option<usize> {
    // 孤立的 ESC 末尾：不完整（与 strip 的孤立 ESC 保留行为对应，由调用方定夺）
    if esc + 1 >= input.len() {
        return None;
    }
    match input[esc + 1] {
        // CSI: ESC [ params intermediates final
        b'[' => consume_csi(&input[esc + 2..]).map(|n| esc + 2 + n),
        // 字符串型：OSC(BEL 或 ST) / DCS / APC / PM / SOS(ST)
        b']' => consume_string_until(&input[esc + 2..], b'\x07').map(|n| esc + 2 + n),
        b'P' | b'_' | b'^' | b'X' => {
            consume_string_until(&input[esc + 2..], 0).map(|n| esc + 2 + n)
        }
        // 其他：ESC + 中间字节(0x20-0x2F)* + 终止字节(0x30-0x7E)
        _ => {
            let mut j = esc + 1;
            while j < input.len() && (0x20..=0x2F).contains(&input[j]) {
                j += 1;
            }
            if j < input.len() && (0x30..=0x7E).contains(&input[j]) {
                Some(j + 1)
            } else if j >= input.len() {
                // ESC + 中间字节后被截断
                None
            } else {
                // 畸形：中间字节后跟非终止字节。与 strip 的畸形分支一致：
                // 序列在中间字节处结束（吞掉 ESC + 中间字节，后续按正文处理）
                Some(j)
            }
        }
    }
}

/// `consume_string_terminator` 的可判定版本：返回 `Some(消费字节数)`（含终止符）
/// 表示序列已结束；`None` 表示到末尾仍未终止（可能被页边界截断）。
///
/// 与 `consume_string_terminator` 的差异仅在返回类型——后者用 `rest.len()`
/// 同时表示"终止符恰好在末尾"和"未终止"，无法区分。
fn consume_string_until(rest: &[u8], bel_terminator: u8) -> Option<usize> {
    let mut i = 0;
    while i < rest.len() {
        if bel_terminator != 0 && rest[i] == bel_terminator {
            return Some(i + 1);
        }
        if rest[i] == 0x1B && i + 1 < rest.len() && rest[i + 1] == b'\\' {
            return Some(i + 2);
        }
        // OSC 遇到单独 ESC（非 ST）：视为该序列在此结束（与 consume_string_terminator 一致）
        if rest[i] == 0x1B && bel_terminator != 0 {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_csi_bracketed_paste() {
        let input = b"\x1b[?2004hhello\x1b[?2004l";
        let out = strip_control_sequences(input);
        assert_eq!(out, b"hello");
    }

    #[test]
    fn strips_csi_color() {
        let input = b"\x1b[31mred\x1b[0m text";
        let out = strip_control_sequences(input);
        assert_eq!(out, b"red text");
    }

    #[test]
    fn strips_csi_el() {
        // EL = Erase in Line: \x1b[K
        let input = b"line\x1b[K";
        let out = strip_control_sequences(input);
        assert_eq!(out, b"line");
    }

    #[test]
    fn strips_osc7_with_bel_terminator() {
        // OSC 7 with BEL terminator
        let input = b"\x1b]7;file://summer/root\x07prompt$ ";
        let out = strip_control_sequences(input);
        assert_eq!(out, b"prompt$ ");
    }

    #[test]
    fn strips_osc_with_st_terminator() {
        // OSC with ST terminator (ESC \)
        let input = b"\x1b]0;title\x1b\\text";
        let out = strip_control_sequences(input);
        assert_eq!(out, b"text");
    }

    #[test]
    fn strips_dcs_sequence() {
        // DCS: ESC P ... ST
        let input = b"\x1bP1$q6c\x1b\\normal";
        let out = strip_control_sequences(input);
        assert_eq!(out, b"normal");
    }

    #[test]
    fn strips_apc_sequence() {
        // APC: ESC _ ... ST
        let input = b"\x1b_hello\x1b\\visible";
        let out = strip_control_sequences(input);
        assert_eq!(out, b"visible");
    }

    #[test]
    fn strips_single_char_escape() {
        // ESC c (RIS - Reset to Initial State)
        let input = b"before\x1b[cafter";
        let out = strip_control_sequences(input);
        assert_eq!(out, b"beforeafter");
    }

    // ── 修复 P1-11a：带中间字节的转义序列必须整体剥离 ────────────────

    #[test]
    fn strips_charset_designation_esc_b() {
        // ESC ( B：G0 字符集指定（less/vim/man/ncurses 高频输出）
        let input = b"a\x1b(Bb\x1b)0c";
        let out = strip_control_sequences(input);
        assert_eq!(out, b"abc");
    }

    #[test]
    fn strips_utf8_charset_esc_percent_g() {
        // ESC % G：选择 UTF-8 字符集
        let input = b"\x1b%Gtext";
        let out = strip_control_sequences(input);
        assert_eq!(out, b"text");
    }

    #[test]
    fn strips_decaln_esc_hash_8() {
        // ESC # 8：DECALN 屏幕校准填充
        let input = b"\x1b#8after";
        let out = strip_control_sequences(input);
        assert_eq!(out, b"after");
    }

    #[test]
    fn strips_bare_esc_c_no_intermediate() {
        // ESC c：单字节终止（无中间字节），RIS
        let input = b"\x1bcafter";
        let out = strip_control_sequences(input);
        assert_eq!(out, b"after");
    }

    // ── 修复 P1-11b：跨页续接剥离（strip_page）───────────────────────

    #[test]
    fn strip_page_complete_input_has_no_trailing() {
        let r = strip_page(b"hello\x1b[31mworld\x1b[0m", &[]);
        assert_eq!(r.text, b"helloworld");
        assert!(r.trailing.is_empty());
    }

    #[test]
    fn strip_page_reassembles_csi_split_across_pages() {
        // CSI 被页边界拦腰截断：第 1 页尾是半个 CSI，第 2 页头是剩余部分
        let page1 = b"ab\x1b[?200";
        let page2 = b"4lhello";
        let r1 = strip_page(page1, &[]);
        assert_eq!(r1.text, b"ab", "页 1 不应把 CSI 尾部泄漏成正文");
        assert_eq!(r1.trailing, b"\x1b[?200");
        let r2 = strip_page(page2, &r1.trailing);
        assert_eq!(r2.text, b"hello");
        assert!(r2.trailing.is_empty());
    }

    #[test]
    fn strip_page_reassembles_osc_split_across_pages() {
        // 未终止的 OSC 吞掉页尾：第 1 页文本只剩 'x'，第 2 页续上后剩 'y'
        let page1 = b"x\x1b]0;tit";
        let page2 = b"le\x07y";
        let r1 = strip_page(page1, &[]);
        assert_eq!(r1.text, b"x");
        assert_eq!(r1.trailing, b"\x1b]0;tit");
        let r2 = strip_page(page2, &r1.trailing);
        assert_eq!(r2.text, b"y");
        assert!(r2.trailing.is_empty());
    }

    #[test]
    fn strip_page_reassembles_split_escape_with_intermediate() {
        // ESC ( B 在页边界截断：ESC + 中间字节在第 1 页，终止字节在第 2 页
        let r1 = strip_page(b"a\x1b(", &[]);
        assert_eq!(r1.text, b"a");
        assert_eq!(r1.trailing, b"\x1b(");
        let r2 = strip_page(b"Bb", &r1.trailing);
        assert_eq!(r2.text, b"b");
    }

    #[test]
    fn strip_page_pending_survives_empty_page() {
        let r1 = strip_page(b"\x1b[?200", &[]);
        assert_eq!(r1.trailing, b"\x1b[?200");
        // 空页（如 cursor 无新数据）：遗留原样保留
        let r2 = strip_page(b"", &r1.trailing);
        assert!(r2.text.is_empty());
        assert_eq!(r2.trailing, b"\x1b[?200");
        let r3 = strip_page(b"4lhi", &r2.trailing);
        assert_eq!(r3.text, b"hi");
    }

    #[test]
    fn strip_page_gives_up_on_oversized_pending() {
        // 未终止的 OSC 超过 MAX_PENDING_BYTES：放弃续接（丢弃尾部，不无限累积）
        let mut page = b"x\x1b]0;".to_vec();
        page.resize(page.len() + MAX_PENDING_BYTES + 1, b'a');
        let r = strip_page(&page, &[]);
        assert_eq!(r.text, b"x");
        assert!(r.trailing.is_empty(), "超限遗留应被放弃");
    }

    #[test]
    fn preserves_plain_text() {
        let input = b"hello world\n";
        let out = strip_control_sequences(input);
        assert_eq!(out, b"hello world\n");
    }

    #[test]
    fn preserves_newlines_and_tabs() {
        let input = b"line1\nline2\r\n\tindented";
        let out = strip_control_sequences(input);
        assert_eq!(out, input);
    }

    #[test]
    fn handles_incomplete_csi_at_end() {
        // CSI without final byte at end of buffer
        let input = b"text\x1b[31";
        let out = strip_control_sequences(input);
        assert_eq!(out, b"text");
    }

    #[test]
    fn handles_isolated_esc_at_end() {
        let input = b"text\x1b";
        let out = strip_control_sequences(input);
        assert_eq!(out, b"text\x1b");
    }

    #[test]
    fn handles_multiple_sequences() {
        // 真实 PTY 输出片段：OSC 7 + bracketed paste + 文本 + color + EL
        let input = b"\x1b]7;file://host/root\x07\x1b[?2004h$ \x1b[31merror\x1b[0m\x1b[K";
        let out = strip_control_sequences(input);
        assert_eq!(out, b"$ error");
    }

    #[test]
    fn empty_input() {
        let out = strip_control_sequences(b"");
        assert!(out.is_empty());
    }

    #[test]
    fn preserves_esc_in_non_sequence_context() {
        // 孤立 ESC 后跟非序列字节：按单字符转义处理，跳过两字节
        // （这是保守行为，PTY 中 ESC 后几乎总是序列）
        let input = b"text\x1b?more";
        let out = strip_control_sequences(input);
        assert_eq!(out, b"textmore");
    }

    #[test]
    fn complex_real_world_output() {
        // 模拟真实 bash 输出：prompt + OSC7 + bracketed paste + 命令 + 结果
        let input = b"\x1b]7;file://host/home/user\x07\x1b[?2004huser@host:~$ \x1b[?2004lls\r\nfile1.txt  file2.txt\r\n\x1b]7;file://host/home/user\x07\x1b[?2004huser@host:~$ \x1b[?2004l";
        let out = strip_control_sequences(input);
        assert_eq!(out, b"user@host:~$ ls\r\nfile1.txt  file2.txt\r\nuser@host:~$ ");
    }
}
