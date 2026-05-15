//! WeChat XML message envelope.
//!
//! Inbound (用户 → 公众号) and outbound (公众号 → 用户) messages are both
//! plain XML. The fields are a fixed set documented under
//! 公众平台开发文档 → 消息管理 → 接收普通消息/事件 / 发送被动响应消息.
//!
//! Only the subset we actually need today is modeled — text + event +
//! image/voice for inbound, text for outbound. Everything else (link,
//! location, video) is preserved as `Other` so the handler can still
//! signature-check and ack it without parsing every leaf field.

use crate::error::{PluginError, Result};
use quick_xml::events::{BytesText, Event};
use quick_xml::reader::Reader;
use std::collections::HashMap;

/// One inbound message as decoded from the POST body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundXml {
    pub to_user_name: String,
    pub from_user_name: String,
    pub create_time: String,
    pub msg_type: String,
    /// Present for `text` messages.
    pub content: Option<String>,
    /// Present for `event` messages — values: subscribe / unsubscribe /
    /// CLICK / VIEW / SCAN.
    pub event: Option<String>,
    /// Present for `event` messages bound to a custom menu key.
    pub event_key: Option<String>,
    /// Message id (text/image/voice/etc.) — used for idempotency.
    pub msg_id: Option<String>,
    /// Raw bag of every other element we didn't classify. Lets the handler
    /// log unfamiliar shapes without crashing.
    pub extra: HashMap<String, String>,
}

/// Parse an inbound XML envelope. Whitespace and CDATA wrappers are both
/// stripped. Unknown elements drop into `extra`.
pub fn parse_inbound(xml: &str) -> Result<InboundXml> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut bag: HashMap<String, String> = HashMap::new();
    let mut current: Option<String> = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                if name == "xml" {
                    continue;
                }
                current = Some(name);
            }
            Ok(Event::Text(t)) => {
                if let Some(name) = &current {
                    let value = decode_text(t).unwrap_or_default();
                    bag.entry(name.clone()).or_insert(value);
                }
            }
            Ok(Event::CData(c)) => {
                if let Some(name) = &current {
                    let value = String::from_utf8_lossy(c.as_ref()).into_owned();
                    bag.insert(name.clone(), value);
                }
            }
            Ok(Event::End(_)) => current = None,
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => return Err(PluginError::BadXml(e.to_string())),
        }
    }
    // Pull out known fields; whatever remains in `bag` lives in `extra`.
    let take = |bag: &mut HashMap<String, String>, k: &str| bag.remove(k);
    let to_user_name = take(&mut bag, "ToUserName")
        .ok_or_else(|| PluginError::BadXml("missing <ToUserName>".into()))?;
    let from_user_name = take(&mut bag, "FromUserName")
        .ok_or_else(|| PluginError::BadXml("missing <FromUserName>".into()))?;
    let create_time = take(&mut bag, "CreateTime")
        .ok_or_else(|| PluginError::BadXml("missing <CreateTime>".into()))?;
    let msg_type = take(&mut bag, "MsgType")
        .ok_or_else(|| PluginError::BadXml("missing <MsgType>".into()))?;
    let content = take(&mut bag, "Content");
    let event = take(&mut bag, "Event");
    let event_key = take(&mut bag, "EventKey");
    let msg_id = take(&mut bag, "MsgId");
    Ok(InboundXml {
        to_user_name,
        from_user_name,
        create_time,
        msg_type,
        content,
        event,
        event_key,
        msg_id,
        extra: bag,
    })
}

/// Build an outbound passive-reply text envelope. `to_user` is the original
/// `FromUserName`, `from_user` is the original `ToUserName` (gh_xxx).
pub fn build_text_reply(to_user: &str, from_user: &str, content: &str) -> String {
    let now = current_unix_seconds();
    format!(
        "<xml>\
<ToUserName><![CDATA[{}]]></ToUserName>\
<FromUserName><![CDATA[{}]]></FromUserName>\
<CreateTime>{}</CreateTime>\
<MsgType><![CDATA[text]]></MsgType>\
<Content><![CDATA[{}]]></Content>\
</xml>",
        escape_cdata(to_user),
        escape_cdata(from_user),
        now,
        escape_cdata(content)
    )
}

/// Defensively neutralize `]]>` inside the body — WeChat clients don't
/// always tolerate it; splitting the marker lets the rest of the text
/// through unharmed.
fn escape_cdata(s: &str) -> String {
    s.replace("]]>", "]]]]><![CDATA[>")
}

fn decode_text(t: BytesText<'_>) -> Result<String> {
    // quick-xml returns the *escaped* bytes for text events; `unescape`
    // resolves &amp; etc. CDATA sections never reach this path.
    let cow = t
        .unescape()
        .map_err(|e| PluginError::BadXml(format!("text unescape: {e}")))?;
    Ok(cow.into_owned())
}

fn current_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_TEXT: &str = r#"
<xml>
  <ToUserName><![CDATA[gh_abc]]></ToUserName>
  <FromUserName><![CDATA[oUser123]]></FromUserName>
  <CreateTime>1700000000</CreateTime>
  <MsgType><![CDATA[text]]></MsgType>
  <Content><![CDATA[你好,小助手]]></Content>
  <MsgId>123456789</MsgId>
</xml>
"#;

    const SAMPLE_SUBSCRIBE: &str = r#"
<xml>
  <ToUserName><![CDATA[gh_abc]]></ToUserName>
  <FromUserName><![CDATA[oUser123]]></FromUserName>
  <CreateTime>1700000001</CreateTime>
  <MsgType><![CDATA[event]]></MsgType>
  <Event><![CDATA[subscribe]]></Event>
</xml>
"#;

    #[test]
    fn parses_text_message() {
        let m = parse_inbound(SAMPLE_TEXT).unwrap();
        assert_eq!(m.msg_type, "text");
        assert_eq!(m.from_user_name, "oUser123");
        assert_eq!(m.content.as_deref(), Some("你好,小助手"));
        assert_eq!(m.msg_id.as_deref(), Some("123456789"));
    }

    #[test]
    fn parses_subscribe_event() {
        let m = parse_inbound(SAMPLE_SUBSCRIBE).unwrap();
        assert_eq!(m.msg_type, "event");
        assert_eq!(m.event.as_deref(), Some("subscribe"));
        assert!(m.content.is_none());
    }

    #[test]
    fn missing_required_field_errors() {
        let bad = "<xml><FromUserName>x</FromUserName></xml>";
        let err = parse_inbound(bad).unwrap_err();
        assert!(matches!(err, PluginError::BadXml(_)));
    }

    #[test]
    fn build_text_reply_round_trips() {
        let xml = build_text_reply("oUser", "gh_abc", "hello");
        let m = parse_inbound(&xml).unwrap();
        assert_eq!(m.msg_type, "text");
        assert_eq!(m.from_user_name, "gh_abc");
        assert_eq!(m.to_user_name, "oUser");
        assert_eq!(m.content.as_deref(), Some("hello"));
    }

    #[test]
    fn build_text_reply_neutralizes_cdata_end_marker() {
        let xml = build_text_reply("a", "b", "evil ]]> payload");
        // The raw `]]>` must not appear unescaped in the CDATA block, otherwise
        // we'd terminate the section early and emit invalid XML.
        assert!(!xml.contains("evil ]]> payload"));
        // The escaped form should be present.
        assert!(xml.contains("]]]]><![CDATA[>"));
    }

    #[test]
    fn extra_bag_captures_unknown_fields() {
        let xml = r#"
<xml>
  <ToUserName>a</ToUserName>
  <FromUserName>b</FromUserName>
  <CreateTime>1</CreateTime>
  <MsgType>text</MsgType>
  <Content>hi</Content>
  <SomeNewField>v</SomeNewField>
</xml>
"#;
        let m = parse_inbound(xml).unwrap();
        assert_eq!(m.extra.get("SomeNewField").map(|s| s.as_str()), Some("v"));
    }
}
