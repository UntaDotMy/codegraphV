//! The prompt-hook gate — the structural-keyword half of `src/directory.ts`.
//!
//! `codegraph prompt-hook` front-loads graph context for a prompt that looks like a structural /
//! flow / impact / "where-how" question. These are the cheap, graph-free checks it runs first.

use codegraph_jsre::Re;
use std::sync::LazyLock;

/// Unicode-aware word-boundary emulation for the keyword lists below. JS's `\b` is ASCII-only —
/// it fires only at `[A-Za-z0-9_]` edges — so it can never bound a keyword whose first or last
/// character is accented or non-Latin: `/\boù\b/` NEVER matches "où est …" (ù isn't an ASCII
/// word char, so no boundary exists next to it). That is the #994 CJK mechanism resurfaced for
/// Latin scripts and Cyrillic (#1126). A lookaround — "not flanked by a letter, digit, or
/// underscore" — is the script-independent equivalent.
const NOT_WORD_BEFORE: &str = r"(?<![\p{L}\p{N}_])";
const NOT_WORD_AFTER: &str = r"(?![\p{L}\p{N}_])";

/// Structural keywords matched as EXACT words (boundary on both sides): short or ambiguous
/// tokens where prefix matching would false-positive ("flow" in "flower", "path" in
/// "pathological"). Grouped by language; a term appears once even when several languages share
/// it. Ambiguous everyday words like PT/ES "como" and DE "wie" are excluded: the hook instead
/// requires another strong keyword, a verified code token, or indexed prose segments (#1654).
const STRUCTURAL_WORDS: &[&str] = &[
    // English — the pre-#1126 list minus what moved to STRUCTURAL_STEMS: the bare-stem entries
    // never matched their own derived forms (`\barchitect\b` can't match "architecture"), and
    // "what calls" is subsumed by the "call" stem.
    "how", "where", "tracing", "flows?", "paths?", "reach(?:es|ed)?", "wired?", "breaks?", "why does",
    // French (où=where, flux=flow, chemin=path, casse=breaks)
    "comment", "où", "flux", "chemins?", "casse",
    // Spanish (cómo=how, dónde/donde=where, flujo=flow, ruta/camino=path, rompe=breaks, llaman /
    // quién llama = call(s) — bare "llama" is excluded: it's also the animal/model name in
    // English prompts)
    "cómo", "dónde", "donde", "flujos?", "rutas?", "caminos?", "rompe", "llaman", "quién llama", "quien llama",
    // Portuguese (onde=where, fluxo=flow, caminho=path)
    "onde", "fluxos?", "caminhos?",
    // German (wo/woher/wohin=where, Pfad=path, Fluss/Ablauf=flow, bricht/kaputt=breaks,
    // ruft=calls, hängt=depends — "hängt … von X ab" splits the separable verb "abhängen", so
    // the "abhäng" stem can't catch it)
    "wo", "woher", "wohin", "pfade?", "fluss", "ablauf", "bricht", "kaputt", "ruft", "hängt",
    // Italian (dove=where, flusso=flow, percorso/i=path)
    "dove", "flusso", "percors[oi]",
    // Russian (как=how, где=where, путь/пути=path, работает=works)
    "как", "где", "путь", "пути", "работает",
    // Ukrainian (як=how, де=where, потік=flow — обліque cases reuse the RU "поток" stem;
    // працює=works)
    "як", "де", "потік", "працює",
    // Dutch (hoe=how, waar=where, roept=calls, werkt=works, aangeroepen=called — the ge-
    // participle escapes the "aanroep" stem)
    "hoe", "waar", "roept", "werkt", "aangeroepen",
    // Polish + Czech (jak=how — shared; gdzie/kde=where, cesta=path)
    "jak", "gdzie", "kde", "cesta",
    // Romanian (cum=how, unde=where; flux is shared with French)
    "cum", "unde",
    // Hungarian (hogyan=how, hol=where)
    "hogyan", "hol",
    // Turkish (nasıl=how, mimari=architecture, takip=trace/follow)
    "nasıl", "mimari", "takip",
    // Indonesian/Malay (bagaimana=how, di mana/dimana=where, alur=flow, jalur=path)
    "bagaimana", "di mana", "dimana", "alur", "jalur",
    // Vietnamese — spaced Latin with heavy diacritics, the exact class ASCII `\b` breaks (làm
    // sao/thế nào=how, ở đâu=where, gọi=call, phụ thuộc=depend, ảnh hưởng=affect, kiến
    // trúc=architecture, cấu trúc=structure, luồng=flow, đường dẫn=path, hoạt động=works, giải
    // thích=explain, theo dõi=trace)
    "làm sao", "thế nào", "ở đâu", "gọi", "phụ thuộc", "ảnh hưởng", "kiến trúc",
    "cấu trúc", "luồng", "đường dẫn", "hoạt động", "giải thích", "theo dõi",
    // Swedish / Danish / Norwegian (hur/hvordan=how, hvor=where, beror=depends, flöde=flow)
    "hur", "hvordan", "hvor", "beror", "flöde",
    // Finnish (miten=how, missä=where, toimii=works)
    "miten", "missä", "toimii",
    // Greek (πώς=how, πού=where — accented forms only: unaccented πως/που are ubiquitous
    // conjunctions; καλεί=calls, δομή=structure, ροή=flow)
    "πώς", "πού", "καλεί", "δομή", "ροή",
    // Hindi (कैसे=how, कहाँ/कहां=where, कॉल=call, निर्भर=depends, संरचना=structure, प्रवाह=flow)
    "कैसे", "कहाँ", "कहां", "कॉल", "निर्भर", "संरचना", "प्रवाह",
];

/// Structural keyword STEMS matched as word PREFIXES (boundary on the left only), so derived
/// forms match without enumerating each: "architect" fires on architecture/architectural,
/// "depend" on depends/dependency/dependencies, "вызыва" on вызывает/вызывается. Mid-word
/// occurrences stay excluded — "restructure"/"independent" don't fire — so precision stays close
/// to the exact-word class. Add a stem only when every plausible completion is still a structural
/// word; a stem with ordinary-English completions must instead enumerate its structural suffixes
/// and re-assert the right boundary (see the four bounded English entries, #1138).
fn structural_stems() -> Vec<String> {
    let mut stems: Vec<String> = [
        // English + the Latin-script languages that share the spelling (French
        // architecture/structure/trace/impact, Spanish depende/implementa/impacto, …). call /
        // trace / affect / connect are NOT safe as open prefixes — callus, calligraphy,
        // Connecticut, connective, affectionate, Tracey are ordinary words that would false-fire
        // the full-explore tier (#1138) — so they carry an enumerated suffix set + right
        // boundary (below). "tracing" lives in STRUCTURAL_WORDS (the e is dropped, so no
        // trace-prefix form matches it).
        "architect", "structur", "depend", "implement", "impact", "explain",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    stems.push(format!("call(?:s|ing|ed|ers?|backs?|able|sites?)?{NOT_WORD_AFTER}"));
    stems.push(format!("trace(?:s|d|rs?)?{NOT_WORD_AFTER}"));
    stems.push(format!("affect(?:s|ed|ing)?{NOT_WORD_AFTER}"));
    stems.push(format!("connect(?:s|ed|ing|ions?|ors?|ivity)?{NOT_WORD_AFTER}"));
    stems.extend(
        [
            // French (appel(le)=call, dépend=depends, implément(e)=implement, connex(ion)=
            // connection, expliqu(e)=explain, fonctionn(e/ement)=works)
            "appel", "dépend", "implément", "connex", "expliqu", "fonctionn",
            // Spanish (llamad(a)=call, afect(a)=affect, conect(a)/conexi(ón)=connect,
            // arquitec(tura)=architecture, estructur(a)=structure, funcion(a)=works,
            // traza(r)=trace, explica=explain)
            "llamad", "afect", "conect", "conexi", "arquitec", "estructur", "funcion", "traza", "explica",
            // Portuguese (chama(da)=call, afeta=affect, arquitet(ura)=architecture,
            // estrutur(a)=structure, quebra(do)=breaks)
            "chama", "afeta", "arquitet", "estrutur", "quebra",
            // German (abhäng(t)=depend, Auswirkung=impact, beeinfluss(t)=affect,
            // verbind(et)=connect, Architektur, Struktur, funktionier(t)=works,
            // Aufruf/aufgerufen=call, erklär(t)=explain, verfolg(en)=trace)
            "abhäng", "auswirkung", "beeinfluss", "verbind", "architekt", "struktur", "funktionier", "aufruf", "aufgerufen", "erklär", "verfolg",
            // Italian (chiam(a/ata)=call, dipend(e/enza)=depend, impatt(o)=impact,
            // connett(e)/conness(ione)=connect, architett(ura), struttur(a),
            // funzion(a/amento)=works, tracci(a)=trace, spiega(mi)=explain)
            "chiam", "dipend", "impatt", "connett", "conness", "architett", "struttur", "funzion", "tracci", "spiega",
            // Russian (вызыва(ет)=calls, завис(ит)=depends, влия(ет)=affects,
            // реализ(ация)=implementation, структур(а), архитектур(а), трассир(овка)=trace,
            // лома(ет)=breaks, объясн(и)=explain, поток=flow)
            "вызыва", "завис", "влия", "реализ", "структур", "архитектур", "трассир", "лома", "объясн", "поток",
            // Ukrainian — і/и spellings diverge from Russian (виклика(є)=calls,
            // залеж(ить)=depends, вплива(є)=affects, архітектур(а), реаліз(ація),
            // поясн(и)=explain, шлях(у)=path; структур(а) is shared with Russian)
            "виклика", "залеж", "вплива", "архітектур", "реаліз", "поясн", "шлях",
            // Dutch (aanroep(en)=call, afhankelijk(heid)=depends, beïnvloed(t)=affects,
            // structuur — "structur" can't reach the uu; uitleg(gen)=explain)
            "aanroep", "afhankelijk", "beïnvloed", "structuur", "uitleg",
            // Polish (wywoł(uje)=calls, zależ(y)=depends, wpływ(a)=affects/impact,
            // przepływ=flow, ścieżk(a)=path, działa(nie)=works, wyjaśni(j)=explain,
            // śledz(enie)=trace; architektura/struktura fire via the German stems)
            "wywoł", "zależ", "wpływ", "przepływ", "ścieżk", "działa", "wyjaśni", "śledz",
            // Czech (volá(ní)=calls, závis(í)=depends, ovlivň(uje)=affects, funguj(e)=works,
            // vysvětl(i)=explain)
            "volá", "závis", "ovlivň", "funguj", "vysvětl",
            // Romanian (apel(ează)=calls, depind(e)=depends — i not e, so "depend" misses it;
            // arhitectur(a) — no c; funcțion(ează)=works, explică=explain)
            "apel", "depind", "arhitectur", "funcțion", "explică",
            // Hungarian (hív(ja)=calls, függ(őség)=depends, működ(ik)=works, struktúr(a) — ú
            // escapes "struktur"; magyaráz(d)=explain; architektúra fires via the German stem)
            "hív", "függ", "működ", "struktúr", "magyaráz",
            // Turkish — agglutinative, so stems beat exact words (nere(de/ye/den)=where,
            // çağır/çağrı=call, bağıml(ı)=depends, bağlant(ı)=connection, akış(ı)=flow,
            // etkile(r)/etkisi=affects/impact)
            "nere", "çağır", "çağrı", "bağıml", "bağlant", "akış", "etkile", "etkisi",
            // Indonesian/Malay — me-/di-/ber- prefixes block a bare stem, so affixed forms are
            // listed too (panggil(an)/memanggil/dipanggil=call, bergantung/tergantung=depends,
            // pengaruh/mempengaruhi/memengaruhi=affect, arsitektur=architecture,
            // fungsi/berfungsi=works, jelaskan/menjelaskan=explain)
            "panggil", "memanggil", "dipanggil", "bergantung", "tergantung", "pengaruh",
            "mempengaruhi", "memengaruhi", "arsitektur", "fungsi", "berfungsi", "jelaskan", "menjelaskan",
            // Swedish / Danish / Norwegian (anrop(ar)=calls, påverk(ar)/påvirk(er)=affects,
            // afhæng(er)/avheng(er)=depends, förklar(a)/forklar=explain, arkitektur — k not ch;
            // funger(ar/er)=works)
            "anrop", "påverk", "påvirk", "afhæng", "avheng", "förklar", "forklar", "arkitektur", "funger",
            // Finnish (kutsu(u)=calls, riippu(u)=depends, arkkitehtuur(i),
            // rakente(en)=structure, selit(ä)=explain)
            "kutsu", "riippu", "arkkitehtuur", "rakente", "selit",
            // Greek — accented and unaccented stem spellings both occur (εξαρτ(άται)=depends,
            // επηρε(άζει)=affects, αρχιτεκτονικ(ή), διαδρομ(ή)=path, εξηγ/εξήγ(ησε)=explain)
            "εξαρτ", "επηρε", "αρχιτεκτονικ", "διαδρομ", "εξηγ", "εξήγ",
            // Hindi (समझा(ओ/इए)=explain, आर्किटेक्चर=architecture)
            "समझा", "आर्किटेक्चर",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    stems
}

static STRUCTURAL_WORDS_RE: LazyLock<Re> =
    LazyLock::new(|| Re::new(&format!("{NOT_WORD_BEFORE}(?:{}){NOT_WORD_AFTER}", STRUCTURAL_WORDS.join("|")), "iu"));
static STRUCTURAL_STEMS_RE: LazyLock<Re> = LazyLock::new(|| Re::new(&format!("{NOT_WORD_BEFORE}(?:{})", structural_stems().join("|")), "iu"));

/// Structural keywords matched as bare SUBSTRINGS, for languages where a boundary can't be relied
/// on: scripts with no word separators (Chinese — simplified AND traditional; the original #994
/// set was simplified-only — Japanese, Thai), Korean (spaced, but particles attach directly to the
/// noun: 구조가/구조를), and Arabic / Farsi / Hebrew (spaced, but proclitics attach to the word:
/// وكيف "and-how", והמבנה "and-the-structure"). JS's `\b` can never fire between Han characters,
/// which was issue #994: the English-only gate silently no-op'd every Chinese prompt, so
/// non-English users got no front-load nudge and no error to explain why.
///
/// KNOWN, ACCEPTED false-positive class (#1140): substring matching cannot see homograph
/// compounds — Korean 구조 (structure) also fires inside 구조대 (rescue squad). Verified unfixable
/// at this layer; the miss rate this design avoids (silently no-op'ing every prompt in these
/// languages, #994) outweighs the occasional off-domain fire.
static STRUCTURAL_UNSEGMENTED: LazyLock<Re> = LazyLock::new(|| {
    Re::new(
        "如何|怎么|怎麼|在哪|哪里|哪裡|追踪|跟踪|追蹤|追跡|トレース|流程|流向|流れ|路径|路徑|経路|调用|調用|呼び出|依赖|依賴|依存|影响|影響|实现|實現|実装|架构|架構|アーキテクチャ|结构|結構|構造|介绍|介紹|解析|分析|原理|机制|機制|仕組み|説明|動作|どうやって|どのように|어떻게|어디|호출|흐름|경로|의존|영향|구현|구조|아키텍처|추적|동작|작동|설명|كيف|أين|اين|يستدعي|استدعاء|يعتمد|تعتمد|يؤثر|تأثير|معماري|بنية|هيكل|تدفق|مسار|تتبع|يعمل|تعمل|اشرح|شرح|چگونه|چطور|کجا|فراخوان|وابسته|تأثیر|معماری|ساختار|مسیر|توضیح|איך|איפה|קורא|תלוי|משפיע|ארכיטקטור|מבנה|זרימה|נתיב|הסבר|อย่างไร|ยังไง|ที่ไหน|เรียกใช้|ขึ้นอยู่กับ|ผลกระทบ|สถาปัตยกรรม|โครงสร้าง|เส้นทาง|ติดตาม|ทำงาน|อธิบาย",
        "",
    )
});

/// Doc/data/asset file extensions — a `name.ext` of this kind is a file reference, not a code
/// symbol, so it must not trip the member-access signal.
static DOC_DATA_EXT: LazyLock<Re> =
    LazyLock::new(|| Re::new(r"\.(md|markdown|txt|rst|json|ya?ml|toml|lock|csv|tsv|log|ini|cfg|conf|env|xml|html?|png|jpe?g|gif|svg|pdf)$", "i"));

/// Does `prompt` contain an explicit structural keyword? A keyword is a strong, self-contained
/// signal, so the front-load hook fires on it directly — no graph check needed. (A *code-token*
/// match, by contrast, is only a candidate the hook verifies against the graph first; see
/// [`extract_code_tokens`].) Coverage is multilingual (#994, #1126): the ~29 languages with the
/// largest developer populations, across Latin, Cyrillic, Greek, CJK, Hangul, Arabic, Hebrew,
/// Thai, and Devanagari scripts. Languages beyond the keyword table still fire through the
/// language-agnostic code-token path.
pub fn has_structural_keyword(prompt: &str) -> bool {
    !prompt.is_empty() && (STRUCTURAL_WORDS_RE.is_match(prompt) || STRUCTURAL_STEMS_RE.is_match(prompt) || STRUCTURAL_UNSEGMENTED.is_match(prompt))
}

/// Identifier-shaped tokens in `prompt` — camelCase / PascalCase-with-inner-cap, snake_case, a
/// `name(` call, or the two sides of an `a.b` member access. Naming a symbol is a code question
/// whatever the surrounding human language, and these shapes almost never occur in ordinary
/// prose, so they catch the common "<symbol> 的调用链?" / "where is <symbol> 定義" prompts no
/// keyword list would.
///
/// These are *candidates*, not a verdict: a tech brand like `JavaScript` or `GitHub` is
/// identifier-shaped too, so the front-load hook checks each token against the actual index and
/// only fires when one is a real symbol here — otherwise a brand-name prompt would inject ~16KB
/// of low-relevance context (issue #994 follow-up). A doc/data filename ("README.md") is excluded
/// from the member-access form since it's a file reference, not a symbol.
pub fn extract_code_tokens(prompt: &str) -> Vec<String> {
    if prompt.is_empty() {
        return Vec::new();
    }
    let mut out: indexmap::IndexSet<String> = indexmap::IndexSet::new();
    // camelCase / PascalCase-with-inner-cap (getUserId, parseToken, UserService) or snake_case
    // (article_publish, get_user) — a whole identifier run that has an inner lower→upper
    // transition or an underscore flanked by alphanumerics.
    static IDENT: LazyLock<Re> = LazyLock::new(|| Re::new(r"[A-Za-z_$][\w$]*", "g"));
    static INNER_CAP: LazyLock<Re> = LazyLock::new(|| Re::new(r"[a-z][A-Z]", ""));
    static SNAKE: LazyLock<Re> = LazyLock::new(|| Re::new(r"[A-Za-z0-9]_[A-Za-z0-9]", ""));
    for m in IDENT.captures_iter(prompt) {
        let w = m.text(0).unwrap_or("");
        if INNER_CAP.is_match(w) || SNAKE.is_match(w) {
            out.insert(w.to_string());
        }
    }
    // call form: an identifier directly before '(' — parseToken(, render(). No whitespace before
    // '(' so prose like "the function (entry point)" doesn't trip it.
    static CALL: LazyLock<Re> = LazyLock::new(|| Re::new(r"([A-Za-z_$][\w$]*)\(", "g"));
    for m in CALL.captures_iter(prompt) {
        if let Some(name) = m.text(1) {
            out.insert(name.to_string());
        }
    }
    // member access on identifiers (user.login) — but not a doc/data filename.
    static MEMBER: LazyLock<Re> = LazyLock::new(|| Re::new(r"([A-Za-z_$][\w$]*)\.([A-Za-z_$][\w$]*)", "g"));
    for m in MEMBER.captures_iter(prompt) {
        if !DOC_DATA_EXT.is_match(m.text(0).unwrap_or("")) {
            out.insert(m.text(1).unwrap_or("").to_string());
            out.insert(m.text(2).unwrap_or("").to_string());
        }
    }
    out.into_iter().collect()
}

/// Cheap, graph-free candidate gate for the front-load hook: could `prompt` be a structural /
/// flow / impact / "where-how" question worth front-loading context for? True on an explicit
/// keyword in any covered language (#994, #1126) OR an identifier-shaped token. A keyword is
/// sufficient to fire on its own; a token-only match is only a candidate the hook then verifies
/// against the graph (a brand name like `JavaScript` is token-shaped but isn't a symbol). Every
/// non-candidate prompt ("fix this typo", in any language) stays a zero-cost no-op.
pub fn is_structural_prompt(prompt: &str) -> bool {
    has_structural_keyword(prompt) || !extract_code_tokens(prompt).is_empty()
}

/// Claude Code persists `UserPromptSubmit` hook stdout above this many characters to a file and
/// shows the model a ~2 KB preview instead (#1694). Measured on Claude Code 2.1.261; documented in
/// the hooks reference as a 10,000-character cap on hook output strings.
pub const CLAUDE_CODE_INLINE_HOOK_OUTPUT_LIMIT: usize = 10_000;

/// Max characters of explore text injected by `codegraph prompt-hook` before truncation. Must stay
/// under [`CLAUDE_CODE_INLINE_HOOK_OUTPUT_LIMIT`] so the host delivers the payload inline. 9,000
/// leaves ~1k for the `<codegraph_context>` wrapper and the `projectPath` nudge lines appended
/// after the cap is applied.
pub const PROMPT_HOOK_INJECTION_MAX: usize = 9_000;

/// Cap explore text for the prompt-hook injection, preserving the existing "call codegraph_explore
/// for the rest" notice when truncated. Lengths are UTF-16 code units, as in the TypeScript.
pub fn cap_prompt_hook_injection(text: &str, max: usize) -> String {
    if codegraph_types::jscompat::utf16_len(text) > max {
        format!("{}\n…(truncated; call codegraph_explore for the rest)", codegraph_types::jscompat::utf16_prefix(text, max))
    } else {
        text.to_string()
    }
}

/// [`cap_prompt_hook_injection`] with the default cap.
pub fn cap_prompt_hook_injection_default(text: &str) -> String {
    cap_prompt_hook_injection(text, PROMPT_HOOK_INJECTION_MAX)
}

/// Claude Code injects `<task-notification>…</task-notification>` blocks as `user` messages when
/// a background agent finishes, and UserPromptSubmit hooks receive them exactly like typed
/// prompts (#1832). The whole prompt must be that single envelope; a user question that merely
/// mentions the marker is still a prompt.
pub fn is_task_notification(prompt: &str) -> bool {
    static RE: LazyLock<Re> = LazyLock::new(|| Re::new(r"^\s*<task-notification>[\s\S]*</task-notification>\s*$", ""));
    RE.is_match(prompt)
}

/// Claude Code hands a subagent's report back to the parent session as a `<agent-message
/// from="…">…</agent-message>` prompt, which UserPromptSubmit hooks also receive (#2184). Same
/// rule as [`is_task_notification`]: only a prompt that is entirely that envelope is skipped.
pub fn is_agent_message(prompt: &str) -> bool {
    static RE: LazyLock<Re> = LazyLock::new(|| Re::new(r"^\s*<agent-message(?:\s[^>]*)?>[\s\S]*</agent-message>\s*$", ""));
    RE.is_match(prompt)
}

/// A message another local Claude Code session sends to this one arrives as a
/// `<cross-session-message from="…">…</cross-session-message>` prompt, which UserPromptSubmit
/// hooks also receive (#2635). Same rule as [`is_task_notification`].
pub fn is_cross_session_message(prompt: &str) -> bool {
    static RE: LazyLock<Re> =
        LazyLock::new(|| Re::new(r"^\s*<cross-session-message(?:\s[^>]*)?>[\s\S]*</cross-session-message>\s*$", ""));
    RE.is_match(prompt)
}

const SYSTEM_REMINDER_CLOSE: &str = "</system-reminder>";

/// The prompt the user typed, without the `<system-reminder>…</system-reminder>` blocks Claude
/// Code's desktop app puts in front of it (#2635): every complete block becomes a newline and the
/// rest is trimmed; a `<system-reminder>` the user only mentions, with no closing tag, stays.
pub fn strip_system_reminders(prompt: &str) -> String {
    // The attribute run stops at `<`, so a tag left open in pasted text can't make the scan
    // quadratic; only the text up to the last closing tag can hold a complete block.
    static BLOCK: LazyLock<Re> =
        LazyLock::new(|| Re::new(r"<system-reminder(?:\s[^<>]*)?>[\s\S]*?</system-reminder>", ""));
    let Some(end) = prompt.rfind(SYSTEM_REMINDER_CLOSE) else {
        return codegraph_types::jscompat::js_trim(prompt).to_string();
    };
    let head_end = end + SYSTEM_REMINDER_CLOSE.len();
    let head = BLOCK.replace_all(&prompt[..head_end], |_| Some("\n".to_string()));
    codegraph_types::jscompat::js_trim(&(head + &prompt[head_end..])).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keywords_across_scripts() {
        assert!(has_structural_keyword("how does login work"));
        assert!(has_structural_keyword("où est la fonction"));
        assert!(has_structural_keyword("登录如何实现"));
        assert!(has_structural_keyword("explain the architecture"));
        assert!(!has_structural_keyword("fix this typo"));
        assert!(!has_structural_keyword("Connecticut callus"));
        assert!(!has_structural_keyword("restructure independent"));
    }

    #[test]
    fn code_tokens() {
        assert_eq!(extract_code_tokens("call parseToken(x) and user.login in README.md"), vec!["parseToken", "user", "login"]);
        assert!(extract_code_tokens("hello world").is_empty());
        assert!(is_structural_prompt("what is getUserId"));
    }

    #[test]
    fn envelopes_and_cap() {
        assert!(is_task_notification("<task-notification>done</task-notification>"));
        assert!(!is_task_notification("see <task-notification>x</task-notification> please"));
        assert!(is_agent_message("<agent-message from=\"a\">hi</agent-message>"));
        assert!(is_cross_session_message("<cross-session-message from=\"s\">x</cross-session-message>"));
        assert!(!is_cross_session_message("about <cross-session-message> tags"));
        assert_eq!(
            strip_system_reminders("<system-reminder>linked</system-reminder>
<system-reminder a=\"1\">x</system-reminder> how does login work "),
            "how does login work"
        );
        assert_eq!(strip_system_reminders(" mention <system-reminder> only "), "mention <system-reminder> only");
        assert!(is_task_notification(&strip_system_reminders(
            "<system-reminder>s</system-reminder><task-notification>done</task-notification>"
        )));
        assert_eq!(cap_prompt_hook_injection("abcdef", 3), "abc\n…(truncated; call codegraph_explore for the rest)");
        assert_eq!(cap_prompt_hook_injection("abc", 3), "abc");
    }
}
