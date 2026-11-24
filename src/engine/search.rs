use std::collections::BTreeSet;

use anyhow::Result;
use tantivy::{
    Index, IndexReader, IndexWriter, ReloadPolicy, Searcher, TantivyDocument, Term,
    collector::TopDocs,
    query::{BooleanQuery, ConstScoreQuery, Query, TermQuery},
    schema::{
        Field, IndexRecordOption, STORED, STRING, Schema, TextFieldIndexing, TextOptions, Value,
    },
};

use super::Item;

pub(super) fn eligible(item: &Item) -> bool {
    item.enabled && item.kind == "answer"
}

// Segment Han text into overlapping bigrams so a query can start in the
// middle of a long phrase. Single-character searches get an explicit unigram.
// Treat other alphanumeric runs as whole words, independent of query syntax.
fn is_han(c: char) -> bool {
    matches!(c as u32, 0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xf900..=0xfaff | 0x20000..=0x323af)
}

fn tokenize(input: &str, indexing: bool, mut emit: impl FnMut(&str)) {
    let mut latin = String::new();
    let mut last_han = None;
    let mut han_len = 0;
    for c in input.chars().chain(std::iter::once(' ')) {
        if is_han(c) {
            if !latin.is_empty() {
                emit(&latin);
                latin.clear();
            }
            if indexing {
                emit(c.encode_utf8(&mut [0; 4]));
            }
            if let Some(prev) = last_han {
                let pair: String = [prev, c].into_iter().collect();
                emit(&pair);
            }
            last_han = Some(c);
            han_len += 1;
        } else {
            if !indexing
                && han_len == 1
                && let Some(last) = last_han
            {
                emit(last.encode_utf8(&mut [0; 4]));
            }
            last_han = None;
            han_len = 0;
            if c.is_alphanumeric() {
                latin.extend(c.to_lowercase());
            } else if !latin.is_empty() {
                emit(&latin);
                latin.clear();
            }
        }
    }
}

#[derive(Clone, Copy)]
struct Fields {
    key: Field,
    id: Field,
    shop: Field,
    product: Field,
    scope: Field,
    text: Field,
}

pub(super) struct FullIndex {
    fields: Fields,
    writer: IndexWriter,
    reader: IndexReader,
}

#[derive(Clone)]
pub(super) struct TextView {
    fields: Fields,
    searcher: Searcher,
}

fn record_key(shop: &str, id: &str) -> String {
    format!("{}:{shop}{id}", shop.len())
}

impl FullIndex {
    pub(super) fn new() -> Result<Self> {
        let mut schema = Schema::builder();
        let text_options = TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("whitespace")
                .set_index_option(IndexRecordOption::WithFreqs),
        );
        let fields = Fields {
            key: schema.add_text_field("key", STRING),
            id: schema.add_text_field("id", STORED),
            shop: schema.add_text_field("shop", STRING),
            product: schema.add_text_field("product", STRING),
            scope: schema.add_text_field("scope", STRING),
            text: schema.add_text_field("text", text_options),
        };
        // SQLite owns the durable copy. Mmap lets the OS reclaim cold inverted
        // index pages and the single writer stays within the two-core budget.
        let index = Index::create_from_tempdir(schema.build())?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        let writer = index.writer_with_num_threads(1, 20_000_000)?;
        Ok(Self {
            fields,
            writer,
            reader,
        })
    }

    pub(super) fn add(&mut self, item: &Item) -> Result<()> {
        if !eligible(item) {
            return Ok(());
        }
        let f = self.fields;
        let mut doc = TantivyDocument::new();
        doc.add_text(f.key, record_key(&item.shop, &item.id));
        doc.add_text(f.id, &item.id);
        doc.add_text(f.shop, &item.shop);
        doc.add_text(f.scope, &item.scope);
        for product in &item.product_ids {
            doc.add_text(f.product, product);
        }
        let mut tokens = String::new();
        for content in [&item.question, &item.answer, &item.model, &item.sku] {
            tokenize(content, true, |term| {
                tokens.push_str(term);
                tokens.push(' ');
            });
        }
        doc.add_text(f.text, tokens);
        self.writer.add_document(doc)?;
        Ok(())
    }

    pub(super) fn delete(&mut self, shop: &str, id: &str) {
        self.writer.delete_term(Term::from_field_text(
            self.fields.key,
            &record_key(shop, id),
        ));
    }

    pub(super) fn publish(&mut self) -> Result<TextView> {
        self.writer.commit()?;
        self.reader.reload()?;
        Ok(TextView {
            fields: self.fields,
            searcher: self.reader.searcher(),
        })
    }
}

fn term(field: Field, value: &str, options: IndexRecordOption) -> Box<dyn Query> {
    Box::new(TermQuery::new(Term::from_field_text(field, value), options))
}

impl TextView {
    pub(super) fn find(
        &self,
        shop: &str,
        product: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<String>> {
        if limit == 0 || shop.is_empty() || query.len() > 4096 {
            return Ok(Vec::new());
        }
        let f = self.fields;
        let mut tokens = BTreeSet::new();
        tokenize(query, false, |word| {
            tokens.insert(word.to_owned());
        });
        if tokens.is_empty() || tokens.len() > 256 {
            return Ok(Vec::new());
        }
        let words: Vec<_> = tokens
            .into_iter()
            .map(|w| term(f.text, &w, IndexRecordOption::WithFreqs))
            .collect();
        let minimum = words.len().div_ceil(3);
        let text = BooleanQuery::union_with_minimum_required_clauses(words, minimum);
        let basic = IndexRecordOption::Basic;
        let mut scopes = vec![term(f.scope, "shop", basic)];
        if !product.is_empty() {
            scopes.push(term(f.product, product, basic));
        }
        let visibility = BooleanQuery::intersection(vec![
            term(f.shop, shop, basic),
            Box::new(BooleanQuery::union(scopes)),
        ]);
        let query = BooleanQuery::intersection(vec![
            Box::new(ConstScoreQuery::new(Box::new(visibility), 0.0)),
            Box::new(text),
        ]);
        let hits = self
            .searcher
            .search(&query, &TopDocs::with_limit(limit).order_by_score())?;
        let mut ids = Vec::with_capacity(hits.len());
        for (_, address) in hits {
            let doc: TantivyDocument = self.searcher.doc(address)?;
            if let Some(value) = doc.get_first(f.id)
                && let Some(id) = value.as_str()
            {
                ids.push(id.to_owned());
            }
        }
        Ok(ids)
    }
}
