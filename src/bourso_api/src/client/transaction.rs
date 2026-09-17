use crate::account::Transaction;
use crate::constants::BASE_URL;

use super::BoursoWebClient;

use anyhow::{Context, Result, bail, ensure};
use chrono::NaiveDate;
use regex::Regex;
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};
use tracing::debug;

impl BoursoWebClient {
    /// Get the transactions for an account over a date range.
    ///
    /// Uses the BoursoBank CSV export endpoint to retrieve transactions.
    ///
    /// # Arguments
    ///
    /// * `account_id` - The account ID (32-character hex string).
    /// * `from_date` - Start date in DD/MM/YYYY format.
    /// * `to_date` - End date in DD/MM/YYYY format.
    ///
    /// # Returns
    ///
    /// The transactions list as a vector of `Transaction`.
    #[cfg(not(tarpaulin_include))]
    pub async fn get_transactions(
        &self,
        account_id: &str,
        from_date: &str,
        to_date: &str,
    ) -> Result<Vec<Transaction>> {
        // The export is a CSRF-protected form POST: first fetch the form page to
        // read its per-session `_token`, then POST the export request.
        let form_page = self
            .client
            .get(format!("{BASE_URL}/mon-budget/generate"))
            .headers(self.get_headers())
            .send()
            .await?
            .text()
            .await?;

        let token = extract_export_token(&form_page)?;

        let response = self
            .client
            .post(format!("{BASE_URL}/budget/exporter-mouvements"))
            // Every input the browser submits, whether or not the server checks it today.
            .form(&[
                ("movementSearch[label]", ""),
                ("movementSearch[selectedAccounts][]", account_id),
                ("movementSearch[fromDate]", from_date),
                ("movementSearch[toDate]", to_date),
                ("movementSearch[format]", "CSV"),
                ("movementSearch[filtredBy]", "filtredByCategory"),
                ("movementSearch[category]", ""),
                ("movementSearch[operationTypes]", ""),
                ("movementSearch[operationType]", ""),
                ("movementSearch[myBudgetPage]", "1"),
                ("movementSearch[_token]", &token),
                ("movementSearch[submit]", ""),
            ])
            .headers(self.get_headers())
            .send()
            .await?;

        // Follow redirects manually (the client uses Policy::none())
        let response = if response.status() == 302 {
            let location = response
                .headers()
                .get("location")
                .context("Missing redirect location header")?
                .to_str()?;
            let redirect_url = if location.starts_with("http") {
                location.to_string()
            } else {
                format!("{BASE_URL}{location}")
            };
            debug!("Following redirect to {}", redirect_url);
            self.client
                .get(&redirect_url)
                .headers(self.get_headers())
                .send()
                .await?
        } else {
            response
        };

        debug!("Export response status: {}", response.status());

        let res = response.bytes().await?;
        let content = String::from_utf8_lossy(&res);
        // Strip BOM if present
        let content = content.strip_prefix('\u{FEFF}').unwrap_or(&content);

        extract_transactions(content)
    }
}

/// Rendered in place of a CSV body when the export filters match no movement.
const NO_MOVEMENTS_MARKER: &str = "Aucune opération ne correspond";

/// Opening tag of the export form, present on the page the export bounces back to.
const EXPORT_FORM_MARKER: &str = r#"name="movementSearch""#;

/// The French labels, in the order BoursoBank emits them.
const FRENCH_HEADER_LABELS: [&str; 12] = [
    "Date Opération",
    "Date Valeur",
    "Libellé",
    "Libellé Suggéré",
    "Catégorie",
    "Catégorie Parente",
    "Solde",
    "Commentaire",
    "Numéro Compte",
    "Libellé Compte",
    "Solde",
    "Pointage",
];

/// BoursoBank relabelled the export columns with French display names. Those names
/// cannot serve as keys — `Solde` titles both the movement amount and the running
/// balance — so the canonical names are recovered by position instead.
const FRENCH_HEADER: [&str; 12] = [
    "dateOp",
    "dateVal",
    "label",
    "suggestedLabel",
    "category",
    "categoryParent",
    "amount",
    "comment",
    "accountNum",
    "accountLabel",
    "accountbalance",
    "mark",
];

/// Scrape the per-session CSRF token out of the export form page.
///
/// The closing quote after `]` keeps this anchored on the attribute name rather than any
/// input whose name merely starts with it, and an empty token is refused here rather than
/// two requests later, where the rejection says nothing about its cause.
fn extract_export_token(page: &str) -> Result<String> {
    Ok(Regex::new(r#"movementSearch\[_token\]"[^>]*?value="(?P<token>[^"]+)""#)
        .expect("valid regex")
        .captures(page)
        .and_then(|c| c.name("token"))
        .context("Could not find a non-empty export form CSRF token")?
        .as_str()
        .to_string())
}

/// Parse a French-formatted amount string to f64.
///
/// Handles thousands separators (spaces and non-breaking spaces) and
/// comma decimal separators as used in BoursoBank CSV exports.
fn parse_amount(s: &str) -> Result<f64> {
    let cleaned = s
        .trim()
        .replace('\u{a0}', "")
        .replace(' ', "")
        .replace(',', ".");
    cleaned
        .parse::<f64>()
        .with_context(|| format!("Unparseable amount {s:?} in export"))
}

/// Extract transactions from a BoursoBank CSV export string.
///
/// # Arguments
///
/// * `content` - The CSV content as a string (without BOM).
///
/// # Returns
///
/// The transactions list as a vector of `Transaction`.
fn extract_transactions(content: &str) -> Result<Vec<Transaction>> {
    // Getting a web page where CSV was requested means either "nothing matched the
    // filters" or that we were bounced (expired session, changed export flow). Only
    // the first is an empty result; treating both as one hid a broken export before.
    //
    // Upstream reads any page carrying the export form as empty. Demanding the
    // no-movements notice as well trades false alarms for the one failure we cannot
    // walk back: the CLI writes its output file unconditionally, so an empty list
    // overwrites months of history with a bare header. An error is always recoverable.
    if content.starts_with("<!DOCTYPE") || content.starts_with("<html") {
        ensure!(
            content.contains(EXPORT_FORM_MARKER) && content.contains(NO_MOVEMENTS_MARKER),
            "Export returned an HTML page instead of CSV, and it is not the export form saying the filters matched nothing; the session has most likely expired"
        );
        return Ok(Vec::new());
    }

    let mut reader = csv::ReaderBuilder::new()
        .delimiter(b';')
        .has_headers(true)
        .flexible(true)
        .from_reader(content.as_bytes());

    // Column positions shift between exports: BoursoBank only emits `tags` when some
    // transaction in the range is tagged, which slides every later column right by one.
    // Reading by position silently mis-parsed amounts, so fields are resolved by name.
    let headers = reader
        .headers()
        .context("Export has no CSV header")?
        .clone();
    let mut names: Vec<String> = headers.iter().map(|header| header.trim().to_string()).collect();
    if names.first().is_some_and(|first| first == FRENCH_HEADER_LABELS[0]) {
        ensure!(
            names.len() == FRENCH_HEADER_LABELS.len(),
            "French export header carries {} columns, expected {}",
            names.len(),
            FRENCH_HEADER_LABELS.len()
        );
        // Recovering names by position is only sound while the positions hold; a rename or
        // a reorder that keeps twelve columns would otherwise read every amount off the
        // running-balance `Solde`, rewriting the figures instead of failing.
        for (index, (found, expected)) in names.iter().zip(FRENCH_HEADER_LABELS).enumerate() {
            ensure!(
                found == expected,
                "French export column {index} is {found:?}, expected {expected:?}"
            );
        }
        names = FRENCH_HEADER.iter().map(|name| name.to_string()).collect();
    }

    let records = reader
        .records()
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("Failed to parse CSV record")?;

    // Tagging any transaction in the range appends a `tags` field to every row, which the
    // French header does not name; unaccounted for, it reads `amount` off `comment`.
    if let Some(width) = records.iter().map(|record| record.len()).max() {
        if width == names.len() + 1 {
            names.insert(3, "tags".to_string());
        }
        ensure!(
            names.len() == width,
            "export header names {} columns but rows carry {}",
            names.len(),
            width
        );
    }

    let column = |name: &str| names.iter().position(|header| header == name);

    let date_op = column("dateOp").context("Export has no `dateOp` column")?;
    let label = column("label").context("Export has no `label` column")?;
    let amount = column("amount").context("Export has no `amount` column")?;
    // The remaining columns are presentational and genuinely absent from some exports.
    let date_val = column("dateVal");
    let category = column("category");
    let category_parent = column("categoryParent");
    // Renamed across export versions; both spellings carry BoursoBank's supplier guess.
    let supplier_found = column("suggestedLabel").or_else(|| column("supplierFound"));
    let comment = column("comment");
    let account_num = column("accountNum");
    let account_label = column("accountLabel");
    let account_balance = column("accountbalance");

    let date_shape = Regex::new(r"^\d{4}-\d{2}-\d{2}$").expect("valid regex");

    records
        .into_iter()
        .map(|record| {
            let field = |index: Option<usize>| {
                index
                    .and_then(|index| record.get(index))
                    .unwrap_or_default()
                    .to_string()
            };
            // Last line of defence against a reordering that keeps the header intact:
            // unlike the amount, a date read off the wrong column still parses as text.
            let operation_date = field(Some(date_op));
            ensure!(
                date_shape.is_match(&operation_date),
                "export column {date_op} holds {operation_date:?}, not a YYYY-MM-DD date"
            );
            Ok(Transaction {
                date_op: operation_date,
                date_val: field(date_val),
                label: field(Some(label)),
                category: field(category),
                category_parent: field(category_parent),
                supplier_found: field(supplier_found),
                amount: parse_amount(
                    record
                        .get(amount)
                        .context("CSV record is missing its amount field")?,
                )?,
                comment: field(comment),
                account_num: field(account_num),
                account_label: field(account_label),
                account_balance: match account_balance.and_then(|index| record.get(index)) {
                    // Absent on the running-balance-less exports; 0.0 is not a real balance.
                    None | Some("") => 0.0,
                    Some(balance) => parse_amount(balance)?,
                },
            })
        })
        .collect()
}

/// A charge that repeats: same merchant, on a steady cadence.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Recurring {
    pub merchant: String,
    pub occurrences: usize,
    /// Distinct calendar months in which the merchant charged.
    pub months: usize,
    /// Median gap between consecutive charges, in days.
    pub every_days: i64,
    pub amount_min: f64,
    pub amount_max: f64,
    pub total: f64,
    pub last: String,
}

impl Recurring {
    pub fn is_fixed(&self) -> bool {
        self.amount_min == self.amount_max
    }
}

/// Find charges that repeat, over transactions already filtered to a date range.
///
/// Two kinds qualify: an identical amount billed repeatedly (a plain subscription),
/// and a merchant billing a varying amount but no more than twice a month (metered
/// services). The latter cap is what keeps everyday spending out — groceries hit the
/// same merchant far more often than any subscription bills.
pub fn recurring(transactions: &[Transaction], min_occurrences: usize) -> Result<Vec<Recurring>> {
    assert!(min_occurrences > 0, "min_occurrences must be positive");

    let spends: Vec<&Transaction> = transactions.iter().filter(|tx| tx.amount < 0.0).collect();

    // Grouping leans on BoursoBank's supplier guess. Without it every card label is
    // unique (each carries its own date and card number), so nothing would ever group
    // and this would answer "no subscriptions" instead of admitting it cannot tell.
    if !spends.is_empty() && spends.iter().all(|tx| tx.supplier_found.trim().is_empty()) {
        bail!("Export carries no supplier labels, so charges cannot be grouped by merchant");
    }

    let mut by_fixed_amount: BTreeMap<(String, i64), Vec<&Transaction>> = BTreeMap::new();
    for tx in &spends {
        let cents = (tx.amount * 100.0).round() as i64;
        by_fixed_amount
            .entry((merchant(tx), cents))
            .or_default()
            .push(tx);
    }

    let mut found = Vec::new();
    let mut claimed: HashSet<(String, i64)> = HashSet::new();
    for ((name, cents), group) in &by_fixed_amount {
        let summary = summarize(name, group)?;
        if qualifies(&summary, min_occurrences) {
            claimed.insert((name.clone(), *cents));
            found.push(summary);
        }
    }

    let mut by_merchant: BTreeMap<String, Vec<&Transaction>> = BTreeMap::new();
    for tx in &spends {
        let cents = (tx.amount * 100.0).round() as i64;
        let name = merchant(tx);
        if claimed.contains(&(name.clone(), cents)) {
            continue;
        }
        by_merchant.entry(name).or_default().push(tx);
    }

    for (name, group) in &by_merchant {
        let summary = summarize(name, group)?;
        if qualifies(&summary, min_occurrences) {
            found.push(summary);
        }
    }

    found.sort_by(|a, b| b.total.abs().total_cmp(&a.total.abs()));
    Ok(found)
}

/// Billing at most twice a month is what separates a subscription from a habit:
/// a merchant you simply shop at turns up far more often than any plan bills.
fn qualifies(summary: &Recurring, min_occurrences: usize) -> bool {
    summary.occurrences >= min_occurrences
        && summary.months >= min_occurrences
        && summary.occurrences <= summary.months * 2
}

/// BoursoBank's own supplier guess collapses "CARTE 20/07/26 LIDL 4147 CB*5287"
/// down to "Lidl", which groups far better than the raw label ever could.
fn merchant(tx: &Transaction) -> String {
    let name = if tx.supplier_found.trim().is_empty() {
        tx.label.trim()
    } else {
        tx.supplier_found.trim()
    };
    name.to_uppercase()
}

fn summarize(name: &str, group: &[&Transaction]) -> Result<Recurring> {
    let mut dates: Vec<NaiveDate> = group
        .iter()
        .map(|tx| {
            NaiveDate::parse_from_str(&tx.date_op, "%Y-%m-%d")
                .with_context(|| format!("Unparseable operation date {:?}", tx.date_op))
        })
        .collect::<Result<_>>()?;
    dates.sort_unstable();

    let mut gaps: Vec<i64> = dates
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).num_days())
        .collect();
    gaps.sort_unstable();

    let amounts: Vec<f64> = group.iter().map(|tx| tx.amount.abs()).collect();
    let months: HashSet<(i32, u32)> = dates
        .iter()
        .map(|date| (chrono::Datelike::year(date), chrono::Datelike::month(date)))
        .collect();

    Ok(Recurring {
        merchant: name.to_string(),
        occurrences: group.len(),
        months: months.len(),
        every_days: gaps.get(gaps.len() / 2).copied().unwrap_or(0),
        amount_min: amounts.iter().copied().fold(f64::INFINITY, f64::min),
        amount_max: amounts.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        total: amounts.iter().sum(),
        last: dates
            .last()
            .expect("group is non-empty by construction")
            .format("%Y-%m-%d")
            .to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_amount() {
        assert_eq!(parse_amount("-568,13").unwrap(), -568.13);
        assert_eq!(parse_amount("1 718,70").unwrap(), 1718.70);
        assert_eq!(parse_amount("-8,99").unwrap(), -8.99);
        assert_eq!(parse_amount("37.29").unwrap(), 37.29);
        assert!(parse_amount("").is_err());
        assert!(parse_amount("n/a").is_err());
    }

    #[test]
    fn test_extract_transactions() {
        let transactions = extract_transactions(TRANSACTIONS_CSV).unwrap();
        assert_eq!(transactions.len(), 3);
        assert_eq!(transactions[0].date_op, "2026-07-24");
        assert_eq!(transactions[0].label, "VIR INST MARTIROS SHAHBAZYAN");
        assert_eq!(transactions[0].amount, 21.81);
        assert_eq!(transactions[0].account_balance, 0.05);
        assert_eq!(transactions[0].category, "Virements reçus");
        assert_eq!(transactions[1].supplier_found, "Cloudflare");
        assert_eq!(transactions[1].amount, -3.94);
        assert_eq!(transactions[2].supplier_found, "Lidl");
        assert_eq!(transactions[2].amount, -13.69);
    }

    /// BoursoBank emits `tags` only when a transaction in the range carries one,
    /// sliding every later column right. Both shapes must parse identically.
    #[test]
    fn test_extract_transactions_ignores_column_shift() {
        let without_tags = extract_transactions(TRANSACTIONS_CSV).unwrap();
        let with_tags = extract_transactions(TRANSACTIONS_CSV_WITH_TAGS).unwrap();

        let amounts = |txs: &[Transaction]| txs.iter().map(|tx| tx.amount).collect::<Vec<_>>();
        let suppliers = |txs: &[Transaction]| {
            txs.iter()
                .map(|tx| tx.supplier_found.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(amounts(&without_tags), amounts(&with_tags));
        assert_eq!(suppliers(&without_tags), suppliers(&with_tags));
    }

    /// The French header titles both the amount and the running balance `Solde`, so a
    /// name lookup cannot tell them apart — reading the amount off the balance would
    /// silently rewrite every figure rather than fail.
    #[test]
    fn test_extract_transactions_french_header() {
        let french = extract_transactions(TRANSACTIONS_CSV_FRENCH).unwrap();
        assert_eq!(french, extract_transactions(TRANSACTIONS_CSV).unwrap());
        assert_eq!(french[0].amount, 21.81);
        assert_eq!(french[0].account_balance, 0.05);
        assert_eq!(french[1].supplier_found, "Cloudflare");
        assert_eq!(french[2].category, "Alimentation");
    }

    /// A row wider than the header by anything other than the unnamed `tags` field means
    /// the export shape moved again; every column past the gap would be read off by one.
    #[test]
    fn test_extract_transactions_rejects_unexplained_extra_columns() {
        let csv = "dateOp;label;amount\n2026-07-24;VIR;21,81;extra;more\n";
        assert!(extract_transactions(csv).is_err());
    }

    #[test]
    fn test_extract_transactions_rejects_missing_amount_column() {
        let csv = "dateOp;label;comment\n2026-07-24;VIR;\n";
        assert!(extract_transactions(csv).is_err());
    }

    /// BoursoBank answers an empty range with the export form, not an empty CSV.
    #[test]
    fn test_extract_transactions_empty_range_is_not_an_error() {
        let html = format!(
            r#"<!DOCTYPE html><html><body><form name="movementSearch">{NO_MOVEMENTS_MARKER} vos filtres de recherche.</form></body></html>"#
        );
        assert!(extract_transactions(&html).unwrap().is_empty());
    }

    /// Any other page (an expired session bouncing us to login) must not read as "no
    /// transactions" — that camouflage is what hid a broken export for so long. Half a
    /// signal is not the signal: the form alone, or the notice alone, is still an error.
    #[test]
    fn test_extract_transactions_rejects_unexpected_html() {
        for html in [
            "<!DOCTYPE html><html><body>Identifiez-vous</body></html>",
            r#"<!DOCTYPE html><html><body><form name="movementSearch"></form></body></html>"#,
            &format!("<!DOCTYPE html><html><body>{NO_MOVEMENTS_MARKER}</body></html>"),
        ] {
            assert!(extract_transactions(html).is_err(), "accepted {html:?}");
        }
    }

    /// Renaming or reordering a column while keeping twelve of them would otherwise read
    /// every amount off the running balance, which parses fine and is silently wrong.
    #[test]
    fn test_extract_transactions_rejects_relabelled_french_header() {
        let reordered = TRANSACTIONS_CSV_FRENCH.replacen("Catégorie", "Genre", 1);
        assert!(extract_transactions(&reordered).is_err());
    }

    /// A reordering that survives the header check still lands a non-date in `dateOp`.
    #[test]
    fn test_extract_transactions_rejects_non_date_operation_date() {
        let csv = "dateOp;label;amount\nBoursoBank;VIR;21,81\n";
        assert!(extract_transactions(csv).is_err());
    }

    /// An empty token is accepted by the form page and only rejected two requests later,
    /// where nothing points back at its cause.
    #[test]
    fn test_extract_export_token() {
        // Attribute order and spacing as BoursoBank serves them.
        let page = |value: &str| {
            format!(
                r#"<form name="movementSearch" method="post" action="/budget/exporter-mouvements">
<input  id="movementSearch_label" type="hidden" class="c-field__input" name="movementSearch[label]" value="" >
<input  id="movementSearch__token" type="hidden" class="c-field__input" data-brs-text-input="data-brs-text-input" name="movementSearch[_token]" value="{value}" >
</form>"#
            )
        };
        assert_eq!(
            extract_export_token(&page("99199c2ad85d9967e8c47268.d1pzVODy5BevlLw")).unwrap(),
            "99199c2ad85d9967e8c47268.d1pzVODy5BevlLw"
        );
        assert!(extract_export_token(&page("")).is_err());
        assert!(extract_export_token("<html><body>no form</body></html>").is_err());
    }

    #[test]
    fn test_recurring_rejects_supplierless_export() {
        let spend = Transaction {
            date_op: "2026-07-01".to_string(),
            label: "CARTE 30/06/26 SOMETHING CB*5287".to_string(),
            amount: -10.0,
            ..Default::default()
        };
        assert!(recurring(&[spend], 3).is_err());
    }

    #[test]
    fn test_recurring_separates_subscriptions_from_shopping() {
        let tx = |date: &str, supplier: &str, amount: f64| Transaction {
            date_op: date.to_string(),
            supplier_found: supplier.to_string(),
            amount,
            ..Default::default()
        };
        let mut transactions = vec![
            // A fixed monthly subscription.
            tx("2026-05-06", "Ekwateur", -51.0),
            tx("2026-06-06", "Ekwateur", -51.0),
            tx("2026-07-06", "Ekwateur", -51.0),
            // Two lines with the same supplier, billed at distinct fixed amounts.
            tx("2026-05-20", "Orange", -29.99),
            tx("2026-06-20", "Orange", -29.99),
            tx("2026-07-20", "Orange", -29.99),
            tx("2026-05-17", "Orange", -15.99),
            tx("2026-06-17", "Orange", -15.99),
            tx("2026-07-17", "Orange", -15.99),
            // Metered billing: monthly, but never the same amount twice.
            tx("2026-05-09", "Cloudflare", -3.94),
            tx("2026-06-09", "Cloudflare", -9.17),
            tx("2026-07-09", "Cloudflare", -3.88),
            // Income must never be reported as a recurring charge.
            tx("2026-05-02", "_", 800.0),
            tx("2026-06-02", "_", 800.0),
            tx("2026-07-02", "_", 800.0),
        ];
        // Groceries: frequent, and a different total every run.
        for (visit, day) in [2, 7, 9, 14, 18, 23, 28].into_iter().enumerate() {
            for (offset, month) in ["05", "06", "07"].into_iter().enumerate() {
                let amount = -(10.0 + visit as f64 * 3.7 + offset as f64 * 1.3);
                transactions.push(tx(&format!("2026-{month}-{day:02}"), "Lidl", amount));
            }
        }

        let found = recurring(&transactions, 3).unwrap();
        let names: Vec<&str> = found.iter().map(|r| r.merchant.as_str()).collect();

        assert!(!names.contains(&"LIDL"), "groceries are not a subscription");
        assert!(!names.contains(&"ELENA"), "income is not a charge");
        assert_eq!(names.iter().filter(|n| **n == "ORANGE").count(), 2);

        let ekwateur = found.iter().find(|r| r.merchant == "EKWATEUR").unwrap();
        assert!(ekwateur.is_fixed());
        assert_eq!(ekwateur.every_days, 31);
        assert_eq!(ekwateur.total, 153.0);

        let cloudflare = found.iter().find(|r| r.merchant == "CLOUDFLARE").unwrap();
        assert!(!cloudflare.is_fixed());
        assert_eq!(cloudflare.amount_min, 3.88);
        assert_eq!(cloudflare.amount_max, 9.17);
        assert_eq!(cloudflare.last, "2026-07-09");
    }

    pub const TRANSACTIONS_CSV: &str = r#"dateOp;dateVal;label;suggestedLabel;category;categoryParent;amount;comment;accountNum;accountLabel;accountbalance;mark
2026-07-24;2026-07-24;"VIR INST MARTIROS SHAHBAZYAN";"Vir Inst Martiros Shahbazyan";"Virements reçus";"Virements reçus";21,81;;00040105299;BoursoBank;0.05;Non
2026-07-22;2026-07-22;"CARTE 20/07/26 CLOUDFLARE CB*5287";Cloudflare;"Non catégorisé";"Non catégorisé";-3,94;;00040105299;BoursoBank;-21.76;Non
2026-07-22;2026-07-22;"CARTE 21/07/26 LIDL 4147 CB*5287";Lidl;Alimentation;"Vie quotidienne";-13,69;;00040105299;BoursoBank;-21.76;Non
"#;

    /// Same rows, but with the `tags` column BoursoBank inserts at index 3.
    pub const TRANSACTIONS_CSV_WITH_TAGS: &str = r#"dateOp;dateVal;label;tags;suggestedLabel;category;categoryParent;amount;comment;accountNum;accountLabel;accountbalance;mark
2026-07-24;2026-07-24;"VIR INST MARTIROS SHAHBAZYAN";;"Vir Inst Martiros Shahbazyan";"Virements reçus";"Virements reçus";21,81;;00040105299;BoursoBank;0.05;Non
2026-07-22;2026-07-22;"CARTE 20/07/26 CLOUDFLARE CB*5287";"???";Cloudflare;"Non catégorisé";"Non catégorisé";-3,94;;00040105299;BoursoBank;-21.76;Non
2026-07-22;2026-07-22;"CARTE 21/07/26 LIDL 4147 CB*5287";;Lidl;Alimentation;"Vie quotidienne";-13,69;;00040105299;BoursoBank;-21.76;Non
"#;

    /// Same rows again, as BoursoBank serves them since August 2026: French display
    /// headers, and a `tags` field in every row that the 12-name header never mentions.
    pub const TRANSACTIONS_CSV_FRENCH: &str = r#""Date Opération";"Date Valeur";Libellé;"Libellé Suggéré";Catégorie;"Catégorie Parente";Solde;Commentaire;"Numéro Compte";"Libellé Compte";Solde;Pointage
2026-07-24;2026-07-24;"VIR INST MARTIROS SHAHBAZYAN";;"Vir Inst Martiros Shahbazyan";"Virements reçus";"Virements reçus";21,81;;00040105299;BoursoBank;0.05;Non
2026-07-22;2026-07-22;"CARTE 20/07/26 CLOUDFLARE CB*5287";"???";Cloudflare;"Non catégorisé";"Non catégorisé";-3,94;;00040105299;BoursoBank;-21.76;Non
2026-07-22;2026-07-22;"CARTE 21/07/26 LIDL 4147 CB*5287";;Lidl;Alimentation;"Vie quotidienne";-13,69;;00040105299;BoursoBank;-21.76;Non
"#;
}
