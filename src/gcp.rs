use crate::config::Config;
use anyhow::anyhow;
use chrono::{NaiveDate, Utc, Datelike};
use fastly::http::StatusCode;
use fastly::{panic_with_status, Body, Error, Request, Response};
use fastly_kv_preview::local_kv::LocalStore;
use hmac_sha256::Hash;
use jwt_simple::algorithms::{RS256KeyPair, RSAKeyPairLike};
use jwt_simple::claims::Claims;
use jwt_simple::prelude::Duration;
use log::error;

#[derive(serde::Serialize, serde::Deserialize, Default, Debug)]
pub struct BqQueryReq {
    kind: String,
    query: String,
    location: String,
    useLegacySql: bool,
}

fn gcp_bq_job_query(
    access_token: &str,
    req_url: &str,
    postbody: BqQueryReq,
) -> Result<String, Error> {
    let mut resp = Request::post(req_url)
        .with_header("Authorization", format!("Bearer {}", access_token))
        .with_body_json(&postbody)?
        .with_pass(true)
        .send("bigquery")?;
    if !resp.get_status().is_success() {
        let resp_str = resp.take_body_str();
        let msg = format!("BQ Query Request error: {}", resp_str);
        error!("{}", msg);
        return Err(anyhow!(msg));
    }
    let resp_str = resp.take_body_str();
    Ok(resp_str)
}

//Service Account to get access token
fn gcp_access_token_request(tomlfile: &Config, scope_value: String) -> Result<String, Error> {
    // open local KV
    let local_store_result = LocalStore::open();
    if local_store_result.is_err() {
        error!("local KV open error");
    }
    // create jwt
    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    struct Scope {
        scope: String,
    }
    let mut access_token = "".to_string();
    if local_store_result.is_ok() {
        access_token = match local_store_result
            .as_ref()
            .unwrap()
            .lookup(&Hash::hash(scope_value.as_bytes()))
        {
            Err(e) => {
                error!("[Warnning] Can NOT lookup local KV: {}", e);
                "".to_string()
            }
            Ok(x) => match x {
                None => "".to_string(),
                Some(x) => x.into_string(),
            },
        };
    };
    let cache_server_host = std::env::var("FASTLY_HOSTNAME").unwrap_or_else(|_| String::new());
    println!("access_token: {} at {}", access_token, cache_server_host);
    if access_token == "".to_string() {
        let scope = Scope {
            scope: scope_value.clone(),
        };
        let claims = Claims::with_custom_claims(scope, Duration::from_secs(3600))
            .with_issuer(&tomlfile.bigquery.service_account_email)
            .with_audience(&tomlfile.gcp.aud);
        let private_key = &tomlfile.bigquery.service_account_key.replace("\\n", "\n");
        let jwt = RS256KeyPair::from_pem(&private_key)?.sign(claims)?;

        // get access token
        #[derive(serde::Serialize, Default, Debug)]
        struct Form {
            grant_type: String,
            assertion: String,
        }
        let form = Form {
            grant_type: tomlfile.gcp.grant_type.to_string(),
            assertion: jwt.to_string(),
        };
        let mut resp = match Request::post(tomlfile.gcp.aud.to_string())
            .with_body_form(&form)?
            .send("idp")
        {
            Err(e) => {
                let msg = format!("Request to Google IDP Error: {}", e);
                error!("{}", msg);
                panic_with_status!(501, "{}", msg);
            }
            Ok(x) => x,
        };
        if !resp.get_status().is_success() {
            let resp_str = resp.take_body_str();
            let msg = format!("Error Access Token!: {}", resp_str);
            error!("{}", msg);
            panic_with_status!(501, "{}", msg);
        }
        let resp_value = resp.take_body_json::<serde_json::Value>()?;
        access_token = resp_value["access_token"]
            .as_str()
            .unwrap_or_else(|| {
                let msg = "Can NOT get gcp access token, logger: {}";
                error!("{}", msg);
                panic_with_status!(501, "{}", msg);
            })
            .to_string();
        let expire = resp_value["expires_in"].as_u64().unwrap_or_else(|| {
            let msg = "Can NOT get gcp access token expires_in";
            error!("{}", msg);
            panic_with_status!(501, "{}", msg);
        });
        if local_store_result.is_ok() {
            if local_store_result.unwrap().insert(
                &Hash::hash(scope_value.as_bytes()),
                Body::from(access_token.clone()),
                std::time::Duration::from_secs(expire),
            ).is_err() {
                let msg = "local kv insert error";
                error!("{}", msg);
            }
        }
    }

    Ok(access_token)
}

pub fn handle_insert_req(
    req: &mut Request,
) -> Result<Response, Error> {
    println!("Start BQ Insert!");
    let tomlfile = Config::load();
    #[derive(serde::Deserialize, Default)]
    struct TopRisingTerms {
        refresh_date: String,
        dma_name: String,
        dma_id: i64,
        term: String,
        week: String,
        score: i64,
        rank: i64,
        percent_gain: i64,
    }
    let top_rising_terms: TopRisingTerms = req.take_body_json::<TopRisingTerms>()?;
    let query = format!(
        "INSERT INTO {}.{} (refresh_date, dma_name, dma_id, term, week, score, rank, percent_gain) VALUES ('{}', '{}', {}, '{}', '{}', {}, {}, {})",
        tomlfile.bigquery.projectid, tomlfile.bigquery.dataset_tableid, top_rising_terms.refresh_date, top_rising_terms.dma_name, top_rising_terms.dma_id, top_rising_terms.term, top_rising_terms.week, top_rising_terms.score, top_rising_terms.rank, top_rising_terms.percent_gain);
    match handle_bq_query_req(&tomlfile, &query) {
        Err(e) => {
            let msg = format!(
                "BQ Insert Error: {}, query: {}",
                e, query
            );
            error!("{}", msg);
            panic_with_status!(501, "{}", msg);
        }
        Ok(x) => x,
    };
    return Ok(Response::from_status(StatusCode::OK));
}

pub fn handle_get_req(req: &Request) -> Result<Response, Error> {
    println!("Start BQ SELECT");
    let tomlfile = Config::load();
    let query_string = match req.get_query::<serde_json::Value>() {
        Err(e) => {
            let msg = format!("Get request, querystring Error: {}", e);
            error!("{}", msg);
            panic_with_status!(501, "{}", msg);
        }
        Ok(x) => x,
    };
    let from_str = query_string["from"].as_str();
    let to_str = query_string["to"].as_str();
    let condition = match (from_str, to_str) {
        (None, None) => "week >= DATE_TRUNC(CURRENT_DATE(), week)".to_string(),
        (Some(x), None) => format!("week >= '{}'", x),
        (None, Some(y)) => {
            let today = Utc::today().naive_utc();
            let to_date = NaiveDate::parse_from_str(&y, "%Y-%m-%d")?;
            let today_weekday = today.weekday().num_days_from_sunday();
            let this_sunday = today.checked_sub_signed(chrono::Duration::days(today_weekday.into())).unwrap();
            if NaiveDate::signed_duration_since(to_date, this_sunday).num_days() < 0 {
                let msg = format!("qurey string `to`:{} is not valid", y);
                error!("{}", msg);
                panic_with_status!(501, "{}", msg);
            }
            format!("week >= DATE_TRUNC(CURRENT_DATE(), week) and week <= '{}'", y)
        },
        (Some(x), Some(y)) => {
            let from_date = NaiveDate::parse_from_str(&x, "%Y-%m-%d")?;
            let to_date = NaiveDate::parse_from_str(&y, "%Y-%m-%d")?;
            if NaiveDate::signed_duration_since(to_date, from_date).num_days() < 0 {
                let msg = format!("qurey string `from`: {} or `to`:{} is not valid", x, y);
                error!("{}", msg);
                panic_with_status!(501, "{}", msg);
            }
            format!("date >= '{}' and date <= '{}'", x, y)
        },
    };
    let query = format!(
                    "SELECT * FROM {}.{} where {}",
                    tomlfile.bigquery.projectid, tomlfile.bigquery.dataset_tableid, condition
                );
    let bqresp_json = match handle_bq_query_req(&tomlfile, &query) {
        Err(e) => {
            let msg = format!("{}, query: {}", e, query);
            error!("{}", msg);
            panic_with_status!(501, "{}", msg);
        }
        Ok(x) => x,
    };
    let fields: Vec<serde_json::Value> = match bqresp_json["schema"]["fields"].as_array() {
        None => {
            let msg = format!("BQ response format doesn't include schema.fields, query: {}", query);
            error!("{}", msg);
            panic_with_status!(501, "{}", msg);
        }
        Some(x) => x.to_vec(),
    };
    let rows: Vec<serde_json::Value> = match bqresp_json["rows"].as_array() {
        None => {
            let msg = format!("There is no rows array in BQ resp, query: {}",query);
            eprintln!("{}", msg);
            let body: serde_json::Value = serde_json::from_str("[]")?;
            return Ok(Response::from_status(StatusCode::OK).with_body_json(&body)?);
        }
        Some(x) => x.to_vec(),
    };
    let mut resp_json: Vec<serde_json::Value> = Vec::new();
    for row in rows {
        let mut data_str = "{".to_string();
        let mut i = 0;
        for field in &fields {
            if field["type"] == "INTEGER" {
                data_str = format!(
                    r#"{} {}:{},"#,
                    data_str,
                    field["name"],
                    row["f"][i]["v"]
                        .as_str()
                        .unwrap_or("0")
                        .parse::<i64>()
                        .unwrap()
                );
            } else {
                let data_decoded = match field["name"].as_str().unwrap() {
                    "update" => urlencoding::decode(row["f"][i]["v"].as_str().unwrap_or(""))?,
                    _ => row["f"][i]["v"].as_str().unwrap_or("").to_string(),
                };
                data_str = format!(
                    r#"{} {}:{},"#,
                    data_str,
                    field["name"],
                    serde_json::to_string::<String>(&data_decoded)?
                );
            }
            i += 1;
        }
        data_str.pop();
        data_str = format!(r#"{}}}"#, data_str);
        println!("{}", data_str);
        let data: serde_json::Value = serde_json::from_str(&data_str)?;
        resp_json.push(data);
    }
    Ok(Response::from_status(StatusCode::OK).with_body_json(&resp_json)?)
}

pub fn handle_bq_query_req(
    tomlfile: &Config,
    query: &str,
) -> Result<serde_json::Value, Error> {
    println!("Start BQ Query");
    // Get Access Token to access BQ.
    let req_url = format!(
        "https://bigquery.googleapis.com/bigquery/v2/projects/{}/queries",
        tomlfile.bigquery.projectid
    );
    let access_token =
        match gcp_access_token_request(&tomlfile, tomlfile.bigquery.scope.to_string()) {
            Err(e) => {
                let msg = format!("Token Request Error: {}", e);
                error!("{}", msg);
                return Err(anyhow!(msg));
            }
            Ok(x) => x,
        };
    // Requesting to BQ
    let querydata = BqQueryReq {
        kind: "bigquery#queryRequest".to_string(),
        query: query.to_string(),
        location: "US".to_string(),
        useLegacySql: false,
    };
    let bqresp_str = match gcp_bq_job_query(&access_token, &req_url, querydata) {
        Err(e) => {
            let msg = format!("BQ Query Request Error: {}", e);
            error!("{}", msg);
            return Err(anyhow!(msg));
        }
        Ok(x) => x,
    };
    let bqresp_json: serde_json::Value = match serde_json::from_str(&bqresp_str) {
        Err(e) => {
            let msg = format!("BQ response format is NOT valid JSON: {}", e);
            eprintln!("{}", msg);
            return Err(anyhow!(msg));
        }
        Ok(x) => x,
    };
    Ok(bqresp_json)
}