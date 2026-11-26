//! Local signals for tool selection. Unclear requests keep the ordinary tool registry.
use icu_normalizer::{ComposingNormalizer, DecomposingNormalizer};
use std::sync::LazyLock;

pub(super) fn normalize(text: &str) -> String {
    if text.is_ascii() {
        return text.to_ascii_lowercase();
    }
    let decomposed = DecomposingNormalizer::new_nfkd().normalize(text);
    let mut latin = false;
    let mut normalized = String::with_capacity(decomposed.len());
    for c in decomposed.chars() {
        if matches!(c, '\u{0300}'..='\u{036f}') {
            if !latin {
                normalized.push(c);
            }
            continue;
        }
        latin = c.is_ascii_alphabetic();
        normalized.extend(
            match c {
                'đ' | 'Đ' => 'd',
                _ => c,
            }
            .to_lowercase(),
        );
    }
    ComposingNormalizer::new_nfc()
        .normalize(&normalized)
        .into_owned()
}

fn untokenized(c: char) -> bool {
    matches!(c,
        '\u{0e00}'..='\u{0eff}' | '\u{1000}'..='\u{109f}' | '\u{1780}'..='\u{17ff}' |
        '\u{3040}'..='\u{30ff}' | '\u{3400}'..='\u{9fff}' | '\u{ac00}'..='\u{d7a3}' |
        '\u{20000}'..='\u{323af}'
    )
}

pub(super) fn contains_phrase(text: &str, phrase: &str) -> bool {
    contains_bounded(text, phrase, |c| !c.is_alphanumeric() || untokenized(c))
}

pub(super) fn contains_identifier(text: &str, identifier: &str) -> bool {
    contains_bounded(text, identifier, |c| {
        (!c.is_alphanumeric() && !matches!(c, '_' | '-')) || untokenized(c)
    })
}

fn contains_bounded(text: &str, phrase: &str, boundary: impl Fn(char) -> bool) -> bool {
    if phrase.is_empty() {
        return false;
    }
    text.match_indices(phrase).any(|(start, hit)| {
        text[..start].chars().next_back().is_none_or(&boundary)
            && text[start + hit.len()..]
                .chars()
                .next()
                .is_none_or(&boundary)
    })
}

pub(super) fn is_greeting(text: &str) -> bool {
    if text.len() > 120 {
        return false;
    }
    let folded = normalize(text);
    let phrase = folded.trim().trim_matches([
        '!', '.', '?', '¡', '¿', '。', '،', '؟', '।', '！', '？', ' ',
    ]);
    let phrase = phrase
        .strip_suffix("aizen")
        .or_else(|| phrase.strip_prefix("aizen"))
        .map_or(phrase, |text| {
            text.trim_matches([' ', ',', '、', '，', '،'])
        });
    matches!(
        phrase,
        "hi" | "hello"
            | "hey"
            | "thanks"
            | "thank you"
            | "xin chao"
            | "chao"
            | "chao ban"
            | "cam on"
            | "cam on ban"
            | "hola"
            | "buenos dias"
            | "gracias"
            | "bonjour"
            | "salut"
            | "merci"
            | "hallo"
            | "guten tag"
            | "danke"
            | "ola"
            | "obrigado"
            | "obrigada"
            | "ciao"
            | "grazie"
            | "merhaba"
            | "tesekkurler"
            | "halo"
            | "terima kasih"
            | "こんにちは"
            | "おはようございます"
            | "ありがとう"
            | "ありがとうございます"
            | "你好"
            | "您好"
            | "谢谢"
            | "謝謝"
            | "안녕하세요"
            | "감사합니다"
            | "привет"
            | "здравствуйте"
            | "спасибо"
            | "مرحبا"
            | "مرحبًا"
            | "السلام عليكم"
            | "شكرا"
            | "شكرًا"
            | "नमस्ते"
            | "धन्यवाद"
            | "สวัสดี"
            | "ขอบคุณ"
            | "שלום"
            | "תודה"
            | "γεια"
            | "ευχαριστω"
            | "ευχαριστώ"
            | "witam"
            | "dziekuje"
    )
}

pub(super) fn is_single_file_read(text: &str) -> bool {
    static READ: LazyLock<regex::Regex> = LazyLock::new(|| {
        let path = r"(?:\x60[^\r\n\x60]+\.[\p{L}\p{N}]{1,12}\x60|(?:[a-z]:)?[\p{L}\p{M}\p{N}_./\\-]+\.[a-z0-9]{1,12})";
        let pattern = [
            r"(?:please )?(?:read|doc|tom tat|summarize|summarise)",
            r"(?:por favor,? )?(?:lee|leer|lea|resume|resuma|leia|resuma o arquivo)",
            r"lis|lisez|lire|resume|resumer|lies|lesen",
            r"leggi|baca|ringkas|oku|ozetle|прочитай|прочитать|اقرا|اقرأ|أقرأ|لخص",
        ]
        .join("|");
        let summary = [
            r"and summarize(?: (?:it|its contents|the contents))?|va tom tat(?: noi dung)?",
            r"y resumelo|e resuma(?: o conteudo)?|et resume(?: son contenu)?",
            r"und fasse (?:ihn|den inhalt) zusammen|и кратко изложи|ولخص محتواه",
        ]
        .join("|");
        let suffix = concat!(
            r"を(?:読んで(?:要約して)?|要約して)(?:ください)?|",
            r"(?:을|를)\s*(?:읽어|요약해)(?:\s*(?:줘|주세요))?|",
            r"\s*(?:पढ़ो|पढ़ें|का सारांश दो)"
        );
        regex::Regex::new(&format!(
            r"^(?:(?:(?:{pattern})\s+|(?:(?:请|請)?(?:读取|讀取|阅读|閱讀|读|讀|总结|總結|摘要)|อ่าน)\s*){path}(?:\s+(?:{summary}))?|{path}(?:{suffix}))\s*[.!?。।]?$"
        )).expect("single-file task pattern")
    });
    text.len() <= 512 && READ.is_match(text)
}

fn has_multiple_steps(text: &str) -> bool {
    [
        "and then",
        "then",
        "after that",
        "va roi",
        "sau do",
        "y luego",
        "despues",
        "puis",
        "ensuite",
        "danach",
        "anschließend",
        "и затем",
        "потом",
        "ثم",
        "然后",
        "然後",
        "再把",
        "その後",
        "それから",
        "次に",
        "그리고 나서",
        "그다음",
        "फिर",
        "จากนั้น",
    ]
    .iter()
    .any(|phrase| contains_phrase(text, phrase))
}

/// A precise repository-content lookup can start with just the search capability. Leave
/// filenames, ambiguous prose and compound requests to the ordinary routing.
pub(super) fn is_repository_lookup(original: &str, folded: &str) -> bool {
    static IDENTIFIER: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"`[\p{L}_][\p{L}\p{N}_]*`|[A-Za-z][A-Za-z0-9]*_[A-Za-z0-9_]+")
            .expect("explicit search identifier")
    });
    (contains_identifier(folded, "search_files")
        || (IDENTIFIER.is_match(original)
            && [
                "find all occurrences",
                "search for",
                "where is",
                "tim moi cho xuat hien",
                "tim tat ca",
                "tim kiem",
                "出現箇所",
                "搜索",
                "搜尋",
                "찾아",
                "busca todas",
                "trouve toutes",
                "suche alle",
                "найди все",
                "ابحث عن",
                "खोजो",
                "ค้นหา",
            ]
            .iter()
            .any(|term| contains_phrase(folded, term))))
        && is_single_clause(folded)
}

pub(crate) fn edits_selected_surface_task(text: &str) -> bool {
    static SOURCE_FILE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(
            r"[\p{L}\p{N}_-]+\.(?:[cm]?js|tsx?|jsx|svelte|vue|rs|py|go|html|css|cs)\b",
        )
        .expect("source file pattern")
    });
    let text = normalize(text);
    let mentions = |terms: &[&str]| terms.iter().any(|term| contains_phrase(&text, term));
    mentions(&[
        "refactor",
        "edit",
        "implement",
        "fix",
        "sua",
        "chinh sua",
        "修正",
        "修复",
        "修改",
        "重构",
        "수정",
        "arregla",
        "corrige",
        "andere",
        "исправь",
        "عدّل",
    ]) && (SOURCE_FILE.is_match(&text)
        || mentions(&[
            "code",
            "ma nguon",
            "workspace",
            "repository",
            "repo",
            "file",
            "tep",
            "ファイル",
            "代码",
            "程式碼",
            "코드",
            "codigo",
            "archivo",
            "datei",
            "файл",
            "الكود",
        ]))
        && mentions(&[
            "browser",
            "selected tab",
            "selected window",
            "trinh duyet",
            "cua so",
            "tab",
            "ブラウザ",
            "ウィンドウ",
            "浏览器",
            "瀏覽器",
            "窗口",
            "브라우저",
            "navegador",
            "navigateur",
            "браузер",
            "المتصفح",
        ])
        && mentions(&[
            "verify",
            "check",
            "test",
            "confirm",
            "kiem tra",
            "xac nhan",
            "確認",
            "検証",
            "测试",
            "验证",
            "확인",
            "verifica",
            "controle",
            "prufe",
            "проверь",
            "تحقق",
        ])
}

pub(super) fn is_short_question(text: &str) -> bool {
    static QUESTION: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(concat!(
        r"^(?:[¿]?\s*(?:what (?:is|are)|define|que es|que son|qu est ce que|qu'est-ce que|",
        r"c'est quoi|was ist|was sind|o que e|cos'e|che cos'e|что такое|ما هو|ما هي|",
        r"apa itu|nedir|什么是|甚麼是|什么叫|什麼叫)\s*[\p{L}\p{M}\p{N}+# -]{2,64}|",
        r"(?:giai thich )?[\p{L}\p{M}\p{N}+# -]{2,64}(?: la gi|とは|とは何ですか|って何|是什么|是什麼|이란 무엇인가요| क्या है|คืออะไร))\s*[.?!。؟।]?$"
    )).expect("definition question pattern")
    });
    if text.len() > 240
        || text.contains(['\n', '\u{0060}', '/', '\\', '@', ':', ';', '；'])
        || has_multiple_steps(text)
        || text
            .trim_end_matches(['。', '।', '?', '؟', '.', '!'])
            .contains('.')
    {
        return false;
    }
    // Paths, commands and compound requests keep the ordinary registry. Discovery remains
    // available to a short question in any script that turns out to need external context.
    QUESTION.is_match(text)
        || (text.ends_with(['?', '؟'])
            && crate::core::turn_shape::classify(text)
                == crate::core::turn_shape::TurnShape::Question)
}

pub(super) fn is_single_clause(text: &str) -> bool {
    let text = text
        .trim()
        .trim_end_matches(['.', '!', '?', '。', '।', '؟']);
    text.chars().count() <= 160
        && !text.contains(['\n', ',', '，', '、', ';', '；', '.', '/', '\\'])
        && !has_multiple_steps(text)
        && !["and", "va", "und", "et", "и"]
            .iter()
            .any(|phrase| contains_phrase(text, phrase))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_aliases_keep_their_scripts_and_identifier_boundaries() {
        assert_eq!(normalize("Ｃrédit số dư"), "credit so du");
        assert_eq!(normalize("ガイド"), normalize("カ\u{3099}イド"));
        assert_eq!(normalize("Счёт"), "счёт");
        assert!(contains_phrase(
            "ledgerの利用可能残高を表示",
            "利用可能残高"
        ));
        assert!(contains_identifier("请使用ledger查询", "ledger"));
        assert!(!contains_identifier("balance_read_all", "balance_read"));
        assert!(!contains_phrase("insufficient", "sufficient"));
    }
}
