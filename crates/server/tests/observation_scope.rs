//! UI observation loss must not erase a mutation receipt or expose another scope's UI.
mod support;
use axum::{body::Body,http::{Request,StatusCode}};
use http_body_util::BodyExt;
use serde_json::{json,Value};
use std::{sync::{Arc,atomic::{AtomicUsize,Ordering}},time::Duration};
use support::{block,build_state_with_runner,mock_runner,MockRunner};
use tower::ServiceExt;
const SECRET:&str="OTHER_SCOPE_PRIVATE_UI";
struct Fixture {state:Arc<server::http::AppState>,source:Arc<tokio::sync::Notify>,action:Arc<tokio::sync::Notify>,homes:Arc<AtomicUsize>,calls:Arc<AtomicUsize>,_runner:MockRunner}
impl Fixture {
 fn new(delayed_source:bool, failed_action:bool)->Self {
  let source=Arc::new(tokio::sync::Notify::new());let read=source.clone();
  let action=Arc::new(tokio::sync::Notify::new());let dispatched=action.clone();
  let homes=Arc::new(AtomicUsize::new(0));let sent=homes.clone();
  let calls=Arc::new(AtomicUsize::new(0));let count=calls.clone();
  let runner=mock_runner(move|request,_| {
   count.fetch_add(1,Ordering::AcqRel);
   if request.starts_with("POST /session "){return Some((Duration::ZERO,json!({"value":{"sessionId":"SESSION"}}).to_string()));}
   if request.contains("/wda/pressButton") {sent.fetch_add(1,Ordering::AcqRel);dispatched.notify_one();
    if failed_action {let body=json!({"value":{"error":"unknown error","message":SECRET}}).to_string();return Some((Duration::from_millis(400),format!("HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len())));}
   }
   if request.contains("/source") {read.notify_one();return Some((if delayed_source{Duration::from_millis(400)}else{Duration::ZERO},json!({"value":{"type":"XCUIElementTypeApplication","label":SECRET,"rect":{"x":0,"y":0,"width":390,"height":844},"children":[{"type":"XCUIElementTypeButton","label":SECRET,"isEnabled":true,"rect":{"x":10,"y":10,"width":100,"height":40}}]}}).to_string()));}
   Some((Duration::ZERO,json!({"value":null}).to_string()))
  });
  Self{state:build_state_with_runner(runner.url()),source,action,homes,calls,_runner:runner}
 }
 async fn response(&self,path:&str,body:Value,owner:&str,takeover:bool)->axum::response::Response {
  let mut req=Request::builder().method("POST").uri(path).header("x-phone-control","1").header("x-phone-owner",owner).header("content-type","application/json");
  if takeover {req=req.header("x-phone-owner-takeover","1");}
  server::http::router(self.state.clone()).oneshot(req.body(Body::from(body.to_string())).unwrap()).await.unwrap()
 }
 async fn transition(&self,owner_change:bool) {
  if owner_change {let r=self.response("/agent/input",json!({"type":"text","text":"invalid","budget_ms":1}),"b",true).await;assert_eq!(r.status(),StatusCode::BAD_REQUEST);}
  else {for body in [json!({"secs":60,"private":true}),json!({"secs":0})] {assert_eq!(self.response("/agent/hold",body,"a",false).await.status(),StatusCode::OK);}}
 }
}
async fn body(r:axum::response::Response)->(StatusCode,Value) {let status=r.status();let data=r.into_body().collect().await.unwrap().to_bytes();(status,serde_json::from_slice(&data).unwrap())}
fn observed(batch:bool)->(&'static str,Value) {if batch{("/agent/actions",json!({"observe":true,"steps":[{"kind":"action","action":{"type":"home"}}]}))}else{("/agent/input?return=delta&settle_ms=1500",json!({"type":"home"}))}}

#[test]
fn a_scope_change_during_final_observation_keeps_the_applied_receipt_without_ui() {block(async {
 for batch in [false,true] {for owner in [false,true] {
  let f=Arc::new(Fixture::new(true,false));let c=f.clone();let (path,payload)=observed(batch);
  let pending=tokio::spawn(async move{body(c.response(path,payload,"a",false).await).await});
  tokio::time::timeout(Duration::from_secs(4),f.source.notified()).await.unwrap();f.transition(owner).await;
  let (status,value)=pending.await.unwrap();assert_eq!(status,StatusCode::OK,"{value}");assert_eq!(value["ok"],true);
  assert!(!value.to_string().contains(SECRET),"an old observation escaped: {value}");
  assert_eq!(value["observation_omitted"],true);if batch{assert_eq!(value["applied_actions"],1);assert_eq!(value["completed"],1);}
  assert_eq!(f.homes.load(Ordering::Acquire),1);assert!(f.state.element_snapshots.lock().unwrap().is_empty());
 }}
});}

#[test]
fn a_scope_change_during_a_wait_stops_later_actions_without_replaying_prior_input() {block(async {
 for owner in [false,true] {
  let f=Arc::new(Fixture::new(true,false));let c=f.clone();
  let pending=tokio::spawn(async move{body(c.response("/agent/actions",json!({"observe":false,"steps":[{"kind":"action","action":{"type":"home"}},{"kind":"wait_for","expect":{"present":[{"label":SECRET}]},"timeout_ms":2000,"poll_ms":50},{"kind":"action","action":{"type":"home"}}]}),"a",false).await).await});
  tokio::time::timeout(Duration::from_secs(4),f.source.notified()).await.unwrap();f.transition(owner).await;
  let (_,value)=pending.await.unwrap();assert_eq!(value["ok"],false,"{value}");assert_eq!(value["applied_actions"],1);assert_eq!(value["completed"],1);assert_eq!(value["failed_step"],1);
  assert_eq!(value["batch_outcome"],"partially_applied");assert_eq!(value["retry_safe"],false);assert_eq!(f.homes.load(Ordering::Acquire),1);
  assert!(!value.to_string().contains(SECRET),"{value}");
 }
});}

#[test]
fn prior_wait_observations_are_removed_when_scope_changes_during_a_later_action() {block(async {
 let f=Arc::new(Fixture::new(false,false));let c=f.clone();
 let pending=tokio::spawn(async move{body(c.response("/agent/actions",json!({"observe":false,"steps":[{"kind":"wait_for","expect":{"present":[{"label":SECRET}]},"timeout_ms":2000,"poll_ms":50},{"kind":"action","action":{"type":"home"},"after_ms":700},{"kind":"action","action":{"type":"home"}}]}),"a",false).await).await});
 tokio::time::timeout(Duration::from_secs(4),f.action.notified()).await.unwrap();f.transition(false).await;
 let (_,value)=pending.await.unwrap();assert_eq!(value["ok"],false,"{value}");assert_eq!(value["completed"],1);assert_eq!(value["failed_step"],1);assert_eq!(value["failed_step_outcome"],"applied");assert_eq!(value["retry_safe"],false);assert_eq!(value["applied_actions"],1);assert_eq!(f.homes.load(Ordering::Acquire),1);assert!(!value.to_string().contains(SECRET),"{value}");
});}

#[test]
fn queued_observation_body_is_redacted_without_dropping_the_receipt() {block(async {
 for batch in [false,true] {
  let f=Fixture::new(false,false);let (path,payload)=observed(batch);let response=f.response(path,payload,"a",false).await;
  assert_eq!(response.status(),StatusCode::OK);f.transition(false).await;
  let (status,value)=body(response).await;assert_eq!(status,StatusCode::OK);assert_eq!(value["ok"],true);assert_eq!(value["observation_omitted"],true);assert!(!value.to_string().contains(SECRET),"{value}");if batch{assert_eq!(value["applied_actions"],1);}
  assert_eq!(f.homes.load(Ordering::Acquire),1);
 }
});}

#[test]
fn uncertain_action_fate_survives_scope_loss_without_its_raw_ui_diagnostic() {block(async {
 let f=Arc::new(Fixture::new(false,true));let c=f.clone();let pending=tokio::spawn(async move{body(c.response("/agent/input",json!({"type":"home"}),"a",false).await).await});
 tokio::time::timeout(Duration::from_secs(4),f.action.notified()).await.unwrap();f.transition(false).await;
 let (status,value)=pending.await.unwrap();assert_eq!(status,StatusCode::BAD_GATEWAY);assert_eq!(value["outcome"],"unknown");assert_eq!(value["retry_safe"],false);assert!(!value.to_string().contains(SECRET),"{value}");assert_eq!(f.homes.load(Ordering::Acquire),1);
});}

#[test]
fn queued_nontext_input_is_refused_before_unlock_or_native_lookup_after_scope_change(){block(async{
 for owner in [false,true]{
  let f=Arc::new(Fixture::new(false,false));let guard=f.state.runner.as_ref().unwrap().lock().await;let c=f.clone();
  let pending=tokio::spawn(async move{body(c.response("/agent/input",json!({"type":"home"}),"a",false).await).await});
  tokio::time::timeout(Duration::from_secs(3),async{loop{if f.state.owner.lock().unwrap().as_ref().is_some_and(|o|o.name=="a"){break;}tokio::task::yield_now().await;}}).await.unwrap();
  f.transition(owner).await;drop(guard);let (status,value)=pending.await.unwrap();
  assert_eq!(status,StatusCode::CONFLICT,"{value}");assert_eq!(value["outcome"],"not_sent");assert_eq!(f.calls.load(Ordering::Acquire),0);
 }
});}
