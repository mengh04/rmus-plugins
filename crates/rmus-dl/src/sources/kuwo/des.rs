//! 酷我 mobi.s 接口的 DES 加密，翻译自 musicdl (CharlesPikachu) 的
//! musicdl/modules/utils/kuwoutils.py，非标准 DES（块按小端组装、尾部
//! 不足 8 字节也作为一块输出），用同文件内的测试向量对拍保证一致。

const MASK32: u64 = (1u64 << 32) - 1;

/// PC2 压缩置换表中的 -1 用 i8 表示并约定跳过。
const ARRAYLS: [u8; 16] = [1, 1, 2, 2, 2, 2, 2, 2, 1, 2, 2, 2, 2, 2, 2, 1];
const ARRAYLSMASK: [u64; 3] = [0, 0x100001, 0x300003];

const ARRAYE: [i8; 64] = [
    31, 0, 1, 2, 3, 4, -1, -1, 3, 4, 5, 6, 7, 8, -1, -1, 7, 8, 9, 10, 11, 12, -1, -1, 11, 12, 13,
    14, 15, 16, -1, -1, 15, 16, 17, 18, 19, 20, -1, -1, 19, 20, 21, 22, 23, 24, -1, -1, 23, 24,
    25, 26, 27, 28, -1, -1, 27, 28, 29, 30, 31, 30, -1, -1,
];
const ARRAYIP1: [i8; 64] = [
    39, 7, 47, 15, 55, 23, 63, 31, 38, 6, 46, 14, 54, 22, 62, 30, 37, 5, 45, 13, 53, 21, 61, 29,
    36, 4, 44, 12, 52, 20, 60, 28, 35, 3, 43, 11, 51, 19, 59, 27, 34, 2, 42, 10, 50, 18, 58, 26,
    33, 1, 41, 9, 49, 17, 57, 25, 32, 0, 40, 8, 48, 16, 56, 24,
];
const ARRAYIP2: [i8; 64] = [
    57, 49, 41, 33, 25, 17, 9, 1, 59, 51, 43, 35, 27, 19, 11, 3, 61, 53, 45, 37, 29, 21, 13, 5,
    63, 55, 47, 39, 31, 23, 15, 7, 56, 48, 40, 32, 24, 16, 8, 0, 58, 50, 42, 34, 26, 18, 10, 2,
    60, 52, 44, 36, 28, 20, 12, 4, 62, 54, 46, 38, 30, 22, 14, 6,
];
const ARRAYP: [i8; 32] = [
    15, 6, 19, 20, 28, 11, 27, 16, 0, 14, 22, 25, 4, 17, 30, 9, 1, 7, 23, 13, 31, 26, 2, 8, 18,
    12, 29, 5, 21, 10, 3, 24,
];
const ARRAYPC1: [i8; 56] = [
    56, 48, 40, 32, 24, 16, 8, 0, 57, 49, 41, 33, 25, 17, 9, 1, 58, 50, 42, 34, 26, 18, 10, 2,
    59, 51, 43, 35, 62, 54, 46, 38, 30, 22, 14, 6, 61, 53, 45, 37, 29, 21, 13, 5, 60, 52, 44, 36,
    28, 20, 12, 4, 27, 19, 11, 3,
];
const ARRAYPC2: [i8; 64] = [
    13, 16, 10, 23, 0, 4, -1, -1, 2, 27, 14, 5, 20, 9, -1, -1, 22, 18, 11, 3, 25, 7, -1, -1, 15,
    6, 26, 19, 12, 1, -1, -1, 40, 51, 30, 36, 46, 54, -1, -1, 29, 39, 50, 44, 32, 47, -1, -1, 43,
    48, 38, 55, 33, 52, -1, -1, 45, 41, 49, 35, 28, 31, -1, -1,
];
const MATRIXNSBOX: [[u8; 64]; 8] = [
    [
        14, 4, 3, 15, 2, 13, 5, 3, 13, 14, 6, 9, 11, 2, 0, 5, 4, 1, 10, 12, 15, 6, 9, 10, 1, 8,
        12, 7, 8, 11, 7, 0, 0, 15, 10, 5, 14, 4, 9, 10, 7, 8, 12, 3, 13, 1, 3, 6, 15, 12, 6, 11,
        2, 9, 5, 0, 4, 2, 11, 14, 1, 7, 8, 13,
    ],
    [
        15, 0, 9, 5, 6, 10, 12, 9, 8, 7, 2, 12, 3, 13, 5, 2, 1, 14, 7, 8, 11, 4, 0, 3, 14, 11,
        13, 6, 4, 1, 10, 15, 3, 13, 12, 11, 15, 3, 6, 0, 4, 10, 1, 7, 8, 4, 11, 14, 13, 8, 0, 6,
        2, 15, 9, 5, 7, 1, 10, 12, 14, 2, 5, 9,
    ],
    [
        10, 13, 1, 11, 6, 8, 11, 5, 9, 4, 12, 2, 15, 3, 2, 14, 0, 6, 13, 1, 3, 15, 4, 10, 14, 9,
        7, 12, 5, 0, 8, 7, 13, 1, 2, 4, 3, 6, 12, 11, 0, 13, 5, 14, 6, 8, 15, 2, 7, 10, 8, 15, 4,
        9, 11, 5, 9, 0, 14, 3, 10, 7, 1, 12,
    ],
    [
        7, 10, 1, 15, 0, 12, 11, 5, 14, 9, 8, 3, 9, 7, 4, 8, 13, 6, 2, 1, 6, 11, 12, 2, 3, 0, 5,
        14, 10, 13, 15, 4, 13, 3, 4, 9, 6, 10, 1, 12, 11, 0, 2, 5, 0, 13, 14, 2, 8, 15, 7, 4, 15,
        1, 10, 7, 5, 6, 12, 11, 3, 8, 9, 14,
    ],
    [
        2, 4, 8, 15, 7, 10, 13, 6, 4, 1, 3, 12, 11, 7, 14, 0, 12, 2, 5, 9, 10, 13, 0, 3, 1, 11,
        15, 5, 6, 8, 9, 14, 14, 11, 5, 6, 4, 1, 3, 10, 2, 12, 15, 0, 13, 2, 8, 5, 11, 8, 0, 15, 7,
        14, 9, 4, 12, 7, 10, 9, 1, 13, 6, 3,
    ],
    [
        12, 9, 0, 7, 9, 2, 14, 1, 10, 15, 3, 4, 6, 12, 5, 11, 1, 14, 13, 0, 2, 8, 7, 13, 15, 5,
        4, 10, 8, 3, 11, 6, 10, 4, 6, 11, 7, 9, 0, 6, 4, 2, 13, 1, 9, 15, 3, 8, 15, 3, 1, 14, 12,
        5, 11, 0, 2, 12, 14, 7, 5, 10, 8, 13,
    ],
    [
        4, 1, 3, 10, 15, 12, 5, 0, 2, 11, 9, 6, 8, 7, 6, 9, 11, 4, 12, 15, 0, 3, 10, 5, 14, 13,
        7, 8, 13, 14, 1, 2, 13, 6, 14, 9, 4, 1, 2, 14, 11, 13, 5, 0, 1, 10, 8, 3, 0, 11, 3, 5, 9,
        4, 15, 2, 7, 8, 12, 15, 10, 7, 6, 12,
    ],
    [
        13, 7, 10, 0, 6, 9, 5, 15, 8, 4, 3, 10, 11, 14, 12, 5, 2, 11, 9, 6, 15, 12, 0, 3, 4, 1,
        14, 13, 1, 2, 7, 8, 1, 2, 12, 15, 10, 4, 0, 3, 13, 14, 6, 9, 7, 8, 9, 6, 15, 1, 5, 12, 3,
        10, 14, 5, 8, 7, 11, 0, 4, 13, 2, 11,
    ],
];

/// 等价于 Python 的 bittransform：out 的第 i 位 = l 的第 arr[i] 位（arr[i] < 0 跳过）。
fn bittransform(arr: &[i8], l: u64) -> u64 {
    let mut out = 0u64;
    for (i, &idx) in arr.iter().enumerate() {
        if idx >= 0 && (l & (1u64 << idx)) != 0 {
            out |= 1u64 << i;
        }
    }
    out
}

fn des64(longs: &[u64; 16], l: u64) -> u64 {
    let full = bittransform(&ARRAYIP2, l);
    let mut p_source = [full as u32, (full >> 32) as u32];
    for long in longs.iter().take(16) {
        let r = bittransform(&ARRAYE, p_source[1] as u64) ^ long;
        let mut p_r = [0u8; 8];
        for (j, slot) in p_r.iter_mut().enumerate() {
            *slot = ((r >> (j * 8)) & 0xFF) as u8;
        }
        let mut s_out: u32 = 0;
        for sbi in (0..8).rev() {
            s_out = (s_out << 4) | (MATRIXNSBOX[sbi][p_r[sbi] as usize] & 0xF) as u32;
        }
        let new = p_source[0] ^ (bittransform(&ARRAYP, s_out as u64) as u32);
        p_source = [p_source[1], new];
    }
    p_source.reverse();
    let out = ((p_source[1] as u64) << 32) | (p_source[0] as u64 & MASK32);
    bittransform(&ARRAYIP1, out)
}

fn subkeys(l: u64, longs: &mut [u64; 16], mode: u32) {
    let mut state = bittransform(&ARRAYPC1, l);
    for i in 0..16 {
        let r = ARRAYLS[i] as u32;
        let mask = ARRAYLSMASK[r as usize];
        state = ((state & mask) << (28 - r)) | ((state & !mask) >> r);
        longs[i] = bittransform(&ARRAYPC2, state);
    }
    if mode == 1 {
        longs.reverse();
    }
}

/// 加密（musicdl 的 crypt mode=0）：块按小端组装；尾部不足 8 字节补零后
/// 仍作为一块加密；整除时也会额外追加一块（与 Python 实现一致）。
pub fn encrypt(msg: &[u8]) -> Vec<u8> {
    let key = b"ylzsxkwm";
    let mut l = 0u64;
    for (i, &b) in key.iter().enumerate() {
        l |= (b as u64) << (i * 8);
    }
    let mut longs = [0u64; 16];
    subkeys(l, &mut longs, 0);

    let j = msg.len() / 8;
    let mut blocks = Vec::with_capacity(j + 1);
    for m in 0..j {
        let mut block = 0u64;
        for n in (0..8).rev() {
            block = (block << 8) | msg[n + m * 8] as u64;
        }
        blocks.push(block);
    }
    // 尾部（或整除时的零块）
    let mut tail = 0u64;
    for (n, &b) in msg[j * 8..].iter().enumerate() {
        tail |= (b as u64) << (n * 8);
    }
    blocks.push(tail);

    let mut out = Vec::with_capacity(blocks.len() * 8);
    for block in blocks {
        let enc = des64(&longs, block);
        for i6 in 0..8 {
            out.push(((enc >> (i6 * 8)) & 0xFF) as u8);
        }
    }
    out
}

/// mobi.s 的 q 参数：DES 加密后 base64 标准编码。
pub fn encrypt_query(query: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(encrypt(query.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // 向量由 musicdl (Python) 的 KuwoMusicClientUtils.encrypt 生成，见
    // /tmp/opencode/des_vectors.json（会话期间生成，硬编码以保证自包含）。
    #[test]
    fn encrypt_matches_musicdl_vectors() {
        let cases: &[(&str, &str)] = &[
            ("a", "1a6df2f9163435d5"),
            ("hello world!!!!!", "c4362414cb7a978f2112b059f374539f6a63488853e41dd1"),
            (
                "12345678",
                "363c3e0d3031a46c6a63488853e41dd1",
            ),
            (
                "rid=228908&type=convert_url2&format=flac",
                "e0f1088310131fdade771816218cb83d348aa21afa9cbaeba5cc23fbdbaeb41dde67eaf0846c71516a63488853e41dd1",
            ),
            (
                "user=0&corp=kuwo&source=kwplayer_ar_5.1.0.0_B_jiakong_vh.apk&p2p=1&type=convert_url2&sig=0&format=flac&rid=228908",
                "dc7c509d65d335d43a45b89c6312cec8702eeb87d5a4aa01cf8dc1b211f844568604f062fbc759746bafcf81eef5301f03be421fda6b291ff02cffc889822f2685b1802bd4fe0113348aa21afa9cbaeb1c45c6e0112566a66ee83630aacee584cb4ddc6d7b297d89f824a3b43d68602c5c20963ac328cb24",
            ),
        ];
        for (input, expected_hex) in cases {
            let got = encrypt(input.as_bytes());
            assert_eq!(hex(&got), *expected_hex, "input: {input}");
        }
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}
