//! Offline structural and cache-reuse verification; no external API requests.
//! Arguments: source.epub cache-directory prompt.txt
use async_trait::async_trait;
use babel_ebook::config::OutputMode;
use babel_ebook::html::{process_document, repair_book_notes};
use babel_ebook::{BabelEbookError, Config, TranslateContext, TranslationCache, Translator};
use kuchiki::traits::TendrilSink;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex,
};

struct Offline {
    contract: Box<dyn Translator>,
    requests: AtomicUsize,
    payloads: Mutex<Vec<String>>,
}
#[async_trait]
impl Translator for Offline {
    fn name(&self) -> String {
        self.contract.name()
    }
    fn cache_identity(&self) -> String {
        self.contract.cache_identity()
    }
    fn max_output_tokens(&self) -> usize {
        3000
    }
    fn fragment_response_format(&self, count: usize) -> Option<serde_json::Value> {
        self.contract.fragment_response_format(count)
    }
    async fn translate(
        &self,
        text: &str,
        _: &TranslateContext<'_>,
    ) -> Result<String, BabelEbookError> {
        assert!(!text.contains("[[BABEL:"));
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.payloads.lock().unwrap().push(text.to_string());
        Ok(format!("測試：{text}"))
    }
    async fn translate_fragments(
        &self,
        text: &str,
        _: &TranslateContext<'_>,
        _: usize,
    ) -> Result<String, BabelEbookError> {
        assert!(!text.contains("[[BABEL:"));
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.payloads.lock().unwrap().push(text.to_string());
        let runs: Vec<String> = serde_json::from_str(text).unwrap();
        Ok(serde_json::json!({"translations":runs.iter().map(|t| format!("測試：{t}")).collect::<Vec<_>>()} ).to_string())
    }
}
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let mut book = babel_ebook::read_epub(std::path::Path::new(&args[1]))?;
    let cache = TranslationCache::new(args[2].clone().into());
    let prompt = std::fs::read_to_string(&args[3])?;
    let config = Config {
        target_lang: "zh-TW".into(),
        source_lang: "en".into(),
        output_mode: OutputMode::Interleaved,
        max_input_tokens: 4000,
        max_output_tokens: 3000,
        temperature: 0.2,
        system_prompt: Some(prompt),
        ..Config::default()
    };
    let mut provider = babel_ebook::ProviderConfig::for_provider("openai");
    provider.default_model = "gpt-5.4-mini-2026-03-17".into();
    provider.max_tokens = 3000;
    provider.temperature = 0.2;
    let model = Offline {
        contract: babel_ebook::get_translator(
            "openai",
            Some(&provider),
            &Config {
                api_key: Some("offline-placeholder".into()),
                model: "gpt-5.4-mini-2026-03-17".into(),
                max_output_tokens: 3000,
                temperature: 0.2,
                ..Config::default()
            },
            false,
        )?,
        requests: AtomicUsize::new(0),
        payloads: Mutex::new(Vec::new()),
    };
    if let Some(selected) = args.get(4) {
        book.chapters.retain(|c| c.href.contains(selected));
    }
    let mut chapter32_requests = 0;
    for (i, c) in book.chapters.iter_mut().enumerate() {
        let before = model.requests.load(Ordering::Relaxed);
        c.content = process_document(
            &c.content,
            &model,
            &config.translation_options(),
            &cache,
            i,
            &c.href,
            None,
            None,
        )
        .await?;
        eprintln!("Verified chapter {i}: {}", c.href);
        if c.href.contains("index_split_032") {
            chapter32_requests = model.requests.load(Ordering::Relaxed) - before;
        }
    }
    let repaired = repair_book_notes(&mut book)?;
    if args.get(4).is_some() {
        std::fs::write("probe-chapter.xhtml", &book.chapters[0].content)?;
    }
    let mut all_ids = std::collections::HashSet::new();
    let mut refs = Vec::new();
    let mut duplicate_ids = 0;
    let mut duplicate_examples = Vec::new();
    for c in &book.chapters {
        let html = std::str::from_utf8(&c.content)?;
        let mut reader = quick_xml::Reader::from_str(html);
        loop {
            if reader.read_event()? == quick_xml::events::Event::Eof {
                break;
            }
        }
        let doc = kuchiki::parse_html().one(html);
        let base = url::Url::parse("https://babel.invalid/")?.join(&c.href)?;
        for n in doc.select("[id]").unwrap() {
            let mut target = base.clone();
            target.set_fragment(n.attributes.borrow().get("id"));
            if !all_ids.insert(target.to_string()) {
                duplicate_ids += 1;
                if duplicate_examples.len() < 8 {
                    duplicate_examples.push(target.to_string());
                }
            }
        }
        for a in doc
            .select("a[role='doc-noteref'], a[role='doc-backlink']")
            .unwrap()
        {
            refs.push(
                base.join(a.attributes.borrow().get("href").unwrap())?
                    .to_string(),
            );
        }
    }
    let broken = refs.iter().filter(|r| !all_ids.contains(*r)).count();
    assert_eq!(broken, 0, "new note links must resolve");
    eprintln!("Duplicate ID examples: {duplicate_examples:?}");
    assert_eq!(duplicate_ids, 0);
    if args.get(4).is_none() {
        assert!(repaired > 0);
    }
    println!(
        "{}",
        serde_json::json!({"chapters": book.chapters.len(), "mock_requests":model.requests.load(Ordering::Relaxed), "chapter32_mock_requests":chapter32_requests,"note_references":repaired,"validated_note_links":refs.len(),"broken_note_links":broken,"duplicate_ids":duplicate_ids,"external_api_requests":0})
    );
    Ok(())
}
