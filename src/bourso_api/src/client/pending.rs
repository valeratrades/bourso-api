use crate::constants::BASE_URL;

use super::{BoursoWebClient, transaction::parse_amount};

use anyhow::{Context, Result, bail, ensure};
use chrono::NaiveDate;
use regex::Regex;
use serde::Serialize;
use tracing::debug;

/// A card payment BoursoBank has authorised but not booked yet. The CSV export never
/// carries these; the account's movement list shows them ahead of the booked ones.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct PendingTransaction {
    /// Authorisation date (YYYY-MM-DD)
    pub date: String,
    pub label: String,
    /// Amount in EUR
    pub amount: f64,
}

impl BoursoWebClient {
    /// Get the not-yet-booked card payments of a checking account.
    #[cfg(not(tarpaulin_include))]
    pub async fn get_pending_transactions(
        &self,
        account_id: &str,
    ) -> Result<Vec<PendingTransaction>> {
        let page = self
            .client
            .get(format!("{BASE_URL}/compte/cav/{account_id}/mouvements"))
            .headers(self.get_headers())
            // without it the page answers with a 302 that ends the session
            .header("x-requested-with", "XMLHttpRequest")
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        extract_pending(&page).inspect_err(|_| debug!("Account movements page: {page}"))
    }
}

/// Walk the movement list's `<li>`s: each date line dates the items under it, and every
/// item says whether it is an authorization. Every other `<li>` is ignored, so the slice
/// an item owns runs to the next date line or item.
fn extract_pending(page: &str) -> Result<Vec<PendingTransaction>> {
    ensure!(
        page.contains("list__movement"),
        "No movement list on the account page; its markup changed"
    );
    let li = Regex::new(r#"<li\b[^>]*\bclass="([^"]*)""#).expect("valid regex");
    let starts: Vec<(usize, bool)> = li
        .captures_iter(page)
        .filter_map(|c| {
            let classes: Vec<&str> = c[1].split_whitespace().collect();
            let start = c.get(0).expect("whole match").start();
            if classes.contains(&"list-operation-date-line") {
                Some((start, true))
            } else if classes.contains(&"list-operation-item") {
                Some((start, false))
            } else {
                None
            }
        })
        .collect();

    let is_auth = Regex::new(r#"\bdata-is-auth="(true|false)""#).expect("valid regex");
    let mut date = None;
    let mut pending = Vec::new();
    for (i, &(start, is_date_line)) in starts.iter().enumerate() {
        let end = starts.get(i + 1).map_or(page.len(), |&(next, _)| next);
        let slice = &page[start..end];
        if is_date_line {
            date = Some(parse_french_date(&text(slice))?);
            continue;
        }
        let open_tag = &slice[..slice.find('>').context("Unterminated movement <li>")?];
        match is_auth
            .captures(open_tag)
            .map(|c| c.get(1).expect("group").as_str())
        {
            Some("true") => {}
            Some(_) => continue,
            None => bail!("Movement without data-is-auth; the account page markup changed"),
        }
        let date = date.context("Pending movement listed before any date line")?;
        let label = element_text(slice, "list__movement--label-user")
            .context("Pending movement without a label")?;
        let amount = element_text(slice, "list-operation-item__amount")
            .with_context(|| format!("Pending movement {label:?} without an amount"))?;
        pending.push(PendingTransaction {
            date: date.format("%Y-%m-%d").to_string(),
            amount: parse_amount(&amount)?,
            label,
        });
    }
    Ok(pending)
}

/// Text of the first element whose class list holds `class`, `None` when absent or empty.
fn element_text(html: &str, class: &str) -> Option<String> {
    let re = Regex::new(&format!(
        r#"(?s)\bclass="(?:[^"]*\s)?{}(?:\s[^"]*)?"[^>]*>(.*?)</(?:div|span)>"#,
        regex::escape(class)
    ))
    .expect("valid regex");
    Some(text(&re.captures(html)?[1])).filter(|t| !t.is_empty())
}

fn text(html: &str) -> String {
    let stripped = Regex::new(r"(?s)<[^>]*>")
        .expect("valid regex")
        .replace_all(html, " ")
        .replace("&nbsp;", " ")
        .replace("&minus;", "\u{2212}")
        .replace("&euro;", "€")
        .replace("&#039;", "'")
        .replace("&#39;", "'")
        .replace("&quot;", "\"")
        .replace("&amp;", "&");
    stripped.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `mardi 20 octobre 2026`, the weekday optional.
fn parse_french_date(s: &str) -> Result<NaiveDate> {
    const MONTHS: [&str; 12] = [
        "janvier",
        "février",
        "mars",
        "avril",
        "mai",
        "juin",
        "juillet",
        "août",
        "septembre",
        "octobre",
        "novembre",
        "décembre",
    ];
    let lower = s.to_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();
    let [day, month, year] = words[words.len().saturating_sub(3)..] else {
        bail!("Unparseable movement date line {s:?}");
    };
    let month = MONTHS
        .iter()
        .position(|m| *m == month)
        .with_context(|| format!("Unknown month in movement date line {s:?}"))?;
    NaiveDate::from_ymd_opt(
        year.parse()
            .with_context(|| format!("Unparseable year in {s:?}"))?,
        month as u32 + 1,
        day.parse()
            .with_context(|| format!("Unparseable day in {s:?}"))?,
    )
    .with_context(|| format!("Impossible date {s:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from a live page: a booked item, then two pending ones under the day they
    /// were authorised.
    const PAGE: &str = r#"<ul class="list__movement" data-operations-list><li class="list-operation-date-line">mercredi 21 octobre 2026</li><li class="list-operation-item"
        data-brs-detail-operation
        data-id="6ad868c26296b730425a2273"
        data-is-auth="false"
        data-operations-item
    ><div class="list-operation-item__block "><div class="list-operation-item__content"><div class="list-operation-item__label"><div class="list-operation-item__label-name"><span class="list__movement--label-user">VIR INST XPOLLENS IZLY</span></div></div><div class="list-operation-item__amount ">
            − 5,00 €
        </div></div></div></li><li class="list-operation-date-line">mardi 20 octobre 2026</li><li class="list-operation-item"
        data-brs-detail-operation
        data-tag-commander-click='pending_authorizations'
        data-is-auth="true"
        data-operations-item
    ><div class="list-operation-item__block " ><div class="list-operation-item__content"><div class="list-operation-item__category-picto--authorization"><span class="c-icon--authorization2" style="color: #3d5d84"></span></div><div class="list-operation-item__label"><div class="list-operation-item__label-name--authorization"><span class="list__movement--label-user" data-operation-label-long>CLOUDFLARE 18889935273 US</span></div><div class="list-operation-item__label-sub"><span class="list-operation-item__category" title="Autorisation paiement en cours CB*5287">Autorisation paiement en cours CB*5287</span></div></div><div class="list-operation-item__amount  list-operation-item__amount--authorization  ">
            − 4,03 €
        </div><div class="list-operation-item__check"></div></div></div></li><li class="list-operation-item"
        data-is-auth="true"
        data-operations-item
    ><div class="list-operation-item__block " ><div class="list-operation-item__content"><div class="list-operation-item__label"><div class="list-operation-item__label-name--authorization"><span class="list__movement--label-user" data-operation-label-long>ZENHUB 18449344313 CA</span></div></div><div class="list-operation-item__amount  list-operation-item__amount--authorization  ">
            − 6,71 €
        </div></div></div></li></ul>"#;

    #[test]
    fn test_extract_pending() {
        assert_eq!(
            extract_pending(PAGE).unwrap(),
            vec![
                PendingTransaction {
                    date: "2026-10-20".to_string(),
                    label: "CLOUDFLARE 18889935273 US".to_string(),
                    amount: -4.03,
                },
                PendingTransaction {
                    date: "2026-10-20".to_string(),
                    label: "ZENHUB 18449344313 CA".to_string(),
                    amount: -6.71,
                },
            ]
        );
    }

    /// Losing the authorization flag would otherwise read as "nothing pending".
    #[test]
    fn test_extract_pending_rejects_unknown_markup() {
        assert!(extract_pending("<html><body>Connexion</body></html>").is_err());
        assert!(extract_pending(&PAGE.replace("data-is-auth", "data-auth")).is_err());
        assert!(extract_pending(&PAGE.replace("mardi 20 octobre", "Hier")).is_err());
    }
}
