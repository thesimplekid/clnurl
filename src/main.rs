//! A mostly reverse-engineered implementation of LNURLPay following <https://bolt.fun/guide/web-services/lnurl/pay>

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use cln_plugin::options::{ConfigOption, Value};
use cln_rpc::model::InvoiceRequest;
use cln_rpc::primitives::{Amount, AmountOrAny};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::net::SocketAddr;
use std::path::PathBuf;
use tokio::io::{stdin, stdout};
use url::Url;
use uuid::Uuid;

use nostr::event::Event;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let plugin = if let Some(plugin) = cln_plugin::Builder::new(stdin(), stdout())
        .option(ConfigOption::new(
            "clnurl_listen",
            Value::String("127.0.0.1:9876".into()),
            "Listen address for the LNURL web server",
        ))
        .option(ConfigOption::new(
            "clnurl_base_address",
            Value::String("http://localhost/".into()),
            "Base path under which the API endpoints are reachable, e.g. \
            https://example.com/lnurl_api means endpoints are reachable as \
            https://example.com/lnurl_api/lnurl and https://example.com/lnurl_api/invoice",
        ))
        .option(ConfigOption::new(
            "clnurl_description",
            Value::String("Gimme money!".into()),
            "Description to be displayed in LNURL",
        ))
        .option(ConfigOption::new(
            "clnurl_nostr_pubkey",
            Value::OptString,
            "Nostr pub key of zapper",
        ))
        .dynamic()
        .start(())
        .await?
    {
        plugin
    } else {
        return Ok(());
    };

    let rpc_socket: PathBuf = plugin.configuration().rpc_file.parse()?;
    let listen_addr: SocketAddr = plugin
        .option("clnurl_listen")
        .expect("Option is defined")
        .as_str()
        .expect("Option is a string")
        .parse()?;

    let api_base_address: Url = plugin
        .option("clnurl_base_address")
        .expect("Option is defined")
        .as_str()
        .expect("Option is a string")
        .parse()?;

    let description = plugin
        .option("clnurl_description")
        .expect("Option is defined")
        .as_str()
        .expect("Option is a string")
        .to_owned();

    let nostr_pubkey = match plugin.option("clnurl_nostr_pubkey") {
        Some(Value::String(pubkey)) => Some(pubkey),
        Some(Value::OptString) => None,
        _ => {
            // Something unexpected happened
            None
        }
    };

    let state = ClnurlState {
        rpc_socket,
        api_base_address,
        description,
        nostr_pubkey,
    };

    let lnurl_service = Router::new()
        .route("/lnurl", get(get_lnurl_struct))
        .route("/invoice", get(get_invoice))
        .with_state(state);

    axum::Server::bind(&listen_addr)
        .serve(lnurl_service.into_make_service())
        .await?;

    Ok(())
}

#[derive(Debug, Clone)]
struct ClnurlState {
    rpc_socket: PathBuf,
    api_base_address: Url,
    description: String,
    nostr_pubkey: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct LnurlResponse {
    min_sendable: AmountWrapper,
    max_sendable: AmountWrapper,
    metadata: String,
    callback: Url,
    tag: LnurlTag,
    allows_nostr: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    nostr_pubkey: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
enum LnurlTag {
    PayRequest,
}

async fn get_lnurl_struct(
    State(state): State<ClnurlState>,
) -> Result<Json<LnurlResponse>, StatusCode> {
    Ok(Json(LnurlResponse {
        min_sendable: AmountWrapper::from_msat(1),
        max_sendable: AmountWrapper::from_msat(100000000000),
        metadata: serde_json::to_string(&vec![vec!["text/plain".to_string(), state.description]])
            .map_err(|_e| StatusCode::INTERNAL_SERVER_ERROR)?,
        callback: state
            .api_base_address
            .join("invoice")
            .expect("Still a valid URL"),
        tag: LnurlTag::PayRequest,
        allows_nostr: state.nostr_pubkey.is_some(),
        nostr_pubkey: state.nostr_pubkey,
    }))
}

#[derive(Serialize, Deserialize)]
struct GetInvoiceParams {
    amount: AmountWrapper,
    nostr: Option<String>,
}

#[derive(Debug)]
struct AmountWrapper(Amount);

impl AmountWrapper {
    pub fn from_msat(msat: u64) -> AmountWrapper {
        AmountWrapper(Amount::from_msat(msat))
    }

    pub fn msat(&self) -> u64 {
        self.0.msat()
    }
}

impl<'de> Deserialize<'de> for AmountWrapper {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let m_sats = u64::deserialize(deserializer)?;
        let amount = Amount::from_msat(m_sats);
        Ok(AmountWrapper(amount))
    }
}

impl Serialize for AmountWrapper {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.msat().serialize(serializer)
    }
}

impl From<AmountWrapper> for Amount {
    fn from(wrapper: AmountWrapper) -> Amount {
        wrapper.0
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetInvoiceResponse {
    pr: String,
    // TODO: find out proper type
    success_action: Option<String>,
    // TODO: find out proper type
    routes: Vec<String>,
}

async fn get_invoice(
    Query(params): Query<GetInvoiceParams>,
    State(state): State<ClnurlState>,
) -> Result<Json<GetInvoiceResponse>, StatusCode> {
    let mut cln_client = cln_rpc::ClnRpc::new(&state.rpc_socket)
        .await
        .map_err(|_e| StatusCode::INTERNAL_SERVER_ERROR)?;

    let description = match &params.nostr {
        Some(d) => {
            let zap_request: Event =
                Event::from_json(d).map_err(|_e| StatusCode::INTERNAL_SERVER_ERROR)?;
            zap_request
                .verify()
                .map_err(|_e| StatusCode::INTERNAL_SERVER_ERROR)?;
            zap_request.as_json()
        }
        None => serde_json::to_string(&vec![vec!["text/plain".to_string(), state.description]])
            .map_err(|_e| StatusCode::INTERNAL_SERVER_ERROR)?,
    };

    let cln_response = cln_client
        .call(cln_rpc::Request::Invoice(InvoiceRequest {
            amount_msat: AmountOrAny::Amount(params.amount.into()),
            description,
            label: Uuid::new_v4().to_string(),
            expiry: None,
            fallbacks: None,
            preimage: None,
            exposeprivatechannels: None,
            cltv: None,
            deschashonly: Some(true),
        }))
        .await
        .map_err(|_e| StatusCode::INTERNAL_SERVER_ERROR)?;

    let invoice = match cln_response {
        cln_rpc::Response::Invoice(invoice_response) => invoice_response.bolt11,
        _ => panic!("CLN returned wrong response kind"),
    };

    Ok(Json(GetInvoiceResponse {
        pr: invoice,
        success_action: None,
        routes: vec![],
    }))
}

#[cfg(test)]
mod tests {

    use std::str::FromStr;

    use super::*;

    #[test]
    fn test_lnurl_response_serialization() {
        let lnurl_response = LnurlResponse {
            min_sendable: AmountWrapper::from_msat(0),
            max_sendable: AmountWrapper::from_msat(1000000),
            metadata: serde_json::to_string(&vec![vec![
                "text/plain".to_string(),
                "Hello world".to_string(),
            ]])
            .unwrap(),
            callback: Url::from_str("http://example.com").unwrap(),
            tag: LnurlTag::PayRequest,
            allows_nostr: true,
            nostr_pubkey: Some(
                "9630f464cca6a5147aa8a35f0bcdd3ce485324e732fd39e09233b1d848238f31".to_string(),
            ),
        };

        assert_eq!("{\"minSendable\":0,\"maxSendable\":1000000,\"metadata\":\"[[\\\"text/plain\\\",\\\"Hello world\\\"]]\",\"callback\":\"http://example.com/\",\"tag\":\"payRequest\",\"allowsNostr\":true,\"nostrPubkey\":\"9630f464cca6a5147aa8a35f0bcdd3ce485324e732fd39e09233b1d848238f31\"}", serde_json::to_string(&lnurl_response).unwrap());
    }
}
