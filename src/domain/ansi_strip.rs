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
//! | 单字符转义 | `ESC c` 等 | `\x1b[c`（RIS 重置） | 第二字符 |
//!
//! 参考：ECMA-48 / xterm ctlseqs
//!
//! ## 不剥离
//!
//! - `\n` / `\r` / `\t` 等可打印控制字符（保留语义）
//! - 其他非 ESC 开头的字节（原样保留）
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
                // 单字符转义序列：ESC + 一个字节（如 ESC c = RIS）
                // 范围 0x30-0x7E（数字/字母/符号），常见：c(=), 7, 8, D, E, H, M, c
                _ => {
                    // 跳过 ESC + next
                    i += 2;
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
