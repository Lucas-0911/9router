use nine_router_core::protocol::{self, Format::*};
use serde_json::{Value, json};

// Fixtures follow open-sse/translator/request/{openai-to-claude,gemini-to-openai}.js
// and concerns/{finishReason,usage}.js. Deliberate stricter failures are asserted below.
#[test]
fn same_protocol_preserves_extensions_and_native_streaming() {
    let body = json!({"model":"old","messages":[],"stream":true,"future":{"a":1}});
    let converted = protocol::request(&body, OpenAi, OpenAi, "new").unwrap();
    assert_eq!(converted["future"], body["future"]);
    assert_eq!(converted["stream"], true);
    assert_eq!(converted["model"], "new");
    let gemini = protocol::request(&body, Gemini, Gemini, "new").unwrap();
    assert!(gemini.get("model").is_none());
    assert_eq!(
        protocol::response(&body, Responses, Responses, "new").unwrap(),
        body
    );
}

#[test]
fn openai_claude_converts_tools_results_images_and_system() {
    let body = json!({"messages":[
        {"role":"system","content":"Be precise."},
        {"role":"user","content":[{"type":"text","text":"Look"},{"type":"image_url","image_url":{"url":"data:image/png;base64,aGVsbG8="}}]},
        {"role":"assistant","content":null,"tool_calls":[{"id":"call_a","type":"function","function":{"name":"lookup","arguments":"{\"q\":\"x\"}"}}]},
        {"role":"tool","tool_call_id":"call_a","content":"{\"ok\":true}"}
    ],"max_tokens":100,"temperature":0.25,
    "tools":[{"type":"function","function":{"name":"lookup","description":"Find it","parameters":{"type":"object","properties":{"q":{"type":"string"}},"required":["q"]}}}],
    "tool_choice":{"type":"function","function":{"name":"lookup"}}});
    let claude = protocol::request(&body, OpenAi, Claude, "claude-test").unwrap();
    assert_eq!(
        claude["system"],
        json!([{"type":"text","text":"Be precise."}])
    );
    assert_eq!(
        claude["messages"][0]["content"][1],
        json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":"aGVsbG8="}})
    );
    assert_eq!(
        claude["messages"][1]["content"][0],
        json!({"type":"tool_use","id":"call_a","name":"lookup","input":{"q":"x"}})
    );
    assert_eq!(claude["messages"][2]["content"][0]["tool_use_id"], "call_a");
    assert_eq!(
        claude["tool_choice"],
        json!({"type":"tool","name":"lookup"})
    );
    assert_eq!(
        claude["tools"][0]["input_schema"],
        body["tools"][0]["function"]["parameters"]
    );
    let restored = protocol::request(&claude, Claude, OpenAi, "gpt-test").unwrap();
    assert_eq!(
        restored["messages"][2]["tool_calls"][0]["function"]["arguments"],
        "{\"q\":\"x\"}"
    );
    assert_eq!(restored["messages"][3]["content"], "{\"ok\":true}");
}

#[test]
fn gemini_results_preserve_falsy_json_and_distinct_calls_without_ids() {
    let body = json!({"contents":[
      {"role":"model","parts":[{"functionCall":{"name":"lookup","args":{"q":1}}},{"functionCall":{"name":"lookup","args":{"q":2}}}]},
      {"role":"user","parts":[{"functionResponse":{"name":"lookup","response":{"result":false}}},{"functionResponse":{"name":"lookup","response":{"result":0}}}]}
    ]});
    let openai = protocol::request(&body, Gemini, OpenAi, "gpt-test").unwrap();
    let calls = openai["messages"][0]["tool_calls"].as_array().unwrap();
    assert_ne!(calls[0]["id"], calls[1]["id"]);
    assert_eq!(openai["messages"][1]["tool_call_id"], calls[0]["id"]);
    assert_eq!(openai["messages"][2]["tool_call_id"], calls[1]["id"]);
    assert_eq!(openai["messages"][1]["content"], "{\"result\":false}");
    assert_eq!(openai["messages"][2]["content"], "{\"result\":0}");
}

#[test]
fn openai_gemini_keeps_call_results_paired_when_ids_are_reused() {
    let body = json!({"messages":[
      {"role":"assistant","tool_calls":[{"id":"a","type":"function","function":{"name":"first","arguments":"{}"}}]},
      {"role":"tool","tool_call_id":"a","content":"false"},
      {"role":"assistant","tool_calls":[{"id":"a","type":"function","function":{"name":"second","arguments":"{}"}}]},
      {"role":"tool","tool_call_id":"a","content":"0"}
    ]});
    let gemini = protocol::request(&body, OpenAi, Gemini, "gemini-test").unwrap();
    let c = &gemini["contents"];
    assert_ne!(
        c[0]["parts"][0]["functionCall"]["id"],
        c[2]["parts"][0]["functionCall"]["id"]
    );
    for (call, result, name, value) in [(0, 1, "first", json!(false)), (2, 3, "second", json!(0))] {
        assert_eq!(
            c[call]["parts"][0]["functionCall"]["id"],
            c[result]["parts"][0]["functionResponse"]["id"]
        );
        assert_eq!(c[result]["parts"][0]["functionResponse"]["name"], name);
        assert_eq!(
            c[result]["parts"][0]["functionResponse"]["response"]["result"],
            value
        );
    }
    assert!(gemini.get("model").is_none());
}

#[test]
fn signed_thinking_round_trips_without_losing_signature() {
    let body = json!({"max_tokens":50,"messages":[{"role":"assistant","content":[
      {"type":"thinking","thinking":"Consider","signature":"signature-a"},
      {"type":"redacted_thinking","data":"opaque"},
      {"type":"text","text":"Done"}
    ]}]});
    let openai = protocol::request(&body, Claude, OpenAi, "gpt-test").unwrap();
    assert_eq!(openai["messages"][0]["reasoning_content"], "Consider");
    let restored = protocol::request(&openai, OpenAi, Claude, "claude-test").unwrap();
    assert_eq!(
        restored["messages"][0]["content"],
        body["messages"][0]["content"]
    );
}

#[test]
fn gemini_thought_and_tool_signatures_round_trip() {
    let body = json!({"contents":[{"role":"model","parts":[
      {"text":"Think","thought":true,"thoughtSignature":"text-sig"},
      {"functionCall":{"id":"x","name":"lookup","args":{}},"thoughtSignature":"call-sig"}
    ]}]});
    let openai = protocol::request(&body, Gemini, OpenAi, "gpt-test").unwrap();
    assert_eq!(openai["messages"][0]["reasoning_content"], "Think");
    let restored = protocol::request(&openai, OpenAi, Gemini, "gemini-test").unwrap();
    assert_eq!(restored["contents"], body["contents"]);
}

#[test]
fn claude_response_preserves_tools_thinking_and_cache_accounting() {
    let body = json!({"id":"msg_a","type":"message","role":"assistant","model":"claude-test",
      "content":[{"type":"thinking","thinking":"Plan","signature":"sig"},{"type":"tool_use","id":"call_a","name":"lookup","input":{"q":1}}],
      "stop_reason":"tool_use","stop_sequence":null,
      "usage":{"input_tokens":5,"output_tokens":3,"cache_read_input_tokens":7,"cache_creation_input_tokens":2}});
    let converted = protocol::response(&body, Claude, OpenAi, "gpt-test").unwrap();
    assert_eq!(converted["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(
        converted["choices"][0]["message"]["reasoning_content"],
        "Plan"
    );
    assert_eq!(
        converted["usage"],
        json!({"prompt_tokens":14,"completion_tokens":3,"total_tokens":17,"prompt_tokens_details":{"cached_tokens":7,"cache_creation_tokens":2}})
    );
    let restored = protocol::response(&converted, OpenAi, Claude, "claude-test").unwrap();
    assert_eq!(restored["content"], body["content"]);
    assert_eq!(restored["usage"], body["usage"]);
}

#[test]
fn gemini_response_counts_thought_tokens_and_preserves_calls() {
    let body = json!({"responseId":"r_a","modelVersion":"gemini-test","candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"why","thought":true},{"functionCall":{"id":"c","name":"lookup","args":{"x":1}}}]},"finishReason":"STOP"}],
      "usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":4,"thoughtsTokenCount":2,"totalTokenCount":16,"cachedContentTokenCount":3}});
    let converted = protocol::response(&body, Gemini, OpenAi, "gpt-test").unwrap();
    assert_eq!(converted["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(converted["usage"]["completion_tokens"], 6);
    assert_eq!(
        converted["usage"]["completion_tokens_details"]["reasoning_tokens"],
        2
    );
    let restored = protocol::response(&converted, OpenAi, Gemini, "gemini-test").unwrap();
    assert_eq!(restored["usageMetadata"], body["usageMetadata"]);
    assert_eq!(
        restored["candidates"][0]["content"],
        body["candidates"][0]["content"]
    );
}

#[test]
fn critical_unsupported_behavior_fails_instead_of_disappearing() {
    let cases: Vec<(Value, _, _)> = vec![
        (json!({"stream":true,"messages":[]}), OpenAi, Claude),
        (
            json!({"messages":[],"response_format":{"type":"json_object"}}),
            OpenAi,
            Claude,
        ),
        (
            json!({"messages":[{"role":"user","content":[{"type":"input_audio","input_audio":{"data":"a"}}]}]}),
            OpenAi,
            Gemini,
        ),
        (
            json!({"messages":[{"role":"assistant","tool_calls":[{"id":"x","type":"function","function":{"name":"f","arguments":"not json"}}]}]}),
            OpenAi,
            Claude,
        ),
        (
            json!({"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"x","content":"bad","is_error":true}]}]}),
            Claude,
            OpenAi,
        ),
        (
            json!({"contents":[],"safetySettings":[{"category":"x","threshold":"BLOCK_NONE"}]}),
            Gemini,
            OpenAi,
        ),
        (json!({"input":"hello"}), Responses, OpenAi),
        (json!({"messages":[]}), OpenAi, Responses),
        (
            json!({"messages":[{"role":"assistant","content":"a","reasoning_content":"unsigned"}]}),
            OpenAi,
            Claude,
        ),
        (
            json!({"messages":[{"role":"tool","tool_call_id":"orphan","content":"x"}]}),
            OpenAi,
            Gemini,
        ),
    ];
    for (body, from, to) in cases {
        let error = protocol::request(&body, from, to, "test")
            .unwrap_err()
            .to_string();
        assert!(error.contains("unsupported protocol conversion"), "{error}");
    }
}

#[test]
fn response_never_selects_one_of_multiple_candidates_silently() {
    let body = json!({"choices":[{"message":{"role":"assistant","content":"one"},"finish_reason":"stop"},{"message":{"role":"assistant","content":"two"},"finish_reason":"stop"}]});
    assert!(protocol::response(&body, OpenAi, Claude, "test").is_err());
}

#[test]
fn standard_openai_null_optional_response_fields_are_accepted() {
    let body = json!({"id":"chat_x","choices":[{"index":0,"message":{"role":"assistant","content":"Hello","refusal":null,"annotations":[],"tool_calls":null},"finish_reason":"stop","logprobs":null}],
        "usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3,"prompt_tokens_details":{"cached_tokens":0,"audio_tokens":0},"completion_tokens_details":{"reasoning_tokens":0,"audio_tokens":0,"accepted_prediction_tokens":0,"rejected_prediction_tokens":0}}});
    let converted = protocol::response(&body, OpenAi, Claude, "claude-test").unwrap();
    assert_eq!(
        converted["content"],
        json!([{"type":"text","text":"Hello"}])
    );
}

#[test]
fn native_content_ordering_is_never_silently_rearranged() {
    for blocks in [
        json!([{"type":"text","text":"before"},{"type":"thinking","thinking":"after","signature":"s"}]),
        json!([{"type":"tool_use","id":"c","name":"f","input":{}},{"type":"text","text":"after"}]),
    ] {
        let body = json!({"messages":[{"role":"assistant","content":blocks}]});
        assert!(protocol::request(&body, Claude, OpenAi, "test").is_err());
    }
    for parts in [
        json!([{"text":"before"},{"text":"after","thought":true}]),
        json!([{"functionCall":{"id":"c","name":"f","args":{}}},{"text":"after"}]),
    ] {
        assert!(
            protocol::request(
                &json!({"contents":[{"role":"model","parts":parts}]}),
                Gemini,
                OpenAi,
                "test"
            )
            .is_err()
        );
    }
}

#[test]
fn gemini_function_response_object_does_not_gain_an_extra_result_wrapper() {
    let body = json!({"contents":[
        {"role":"model","parts":[{"functionCall":{"id":"c","name":"f","args":{}}}]},
        {"role":"user","parts":[{"functionResponse":{"id":"c","name":"f","response":{"result":false,"count":0}}}]}
    ]});
    let openai = protocol::request(&body, Gemini, OpenAi, "test").unwrap();
    let converted = protocol::request(&openai, OpenAi, Gemini, "test").unwrap();
    assert_eq!(converted["contents"], body["contents"]);
}

#[test]
fn gemini_schemas_do_not_confuse_property_names_with_schema_keywords() {
    let body = json!({"contents":[],"tools":[{"functionDeclarations":[{"name":"f","parameters":{"type":"OBJECT","properties":{"type":{"type":"STRING","enum":["OBJECT"]},"nullable":{"type":"BOOLEAN"}},"required":["type"]}}]}]});
    let converted = protocol::request(&body, Gemini, OpenAi, "test").unwrap();
    assert_eq!(
        converted["tools"][0]["function"]["parameters"],
        json!({"type":"object","properties":{"type":{"type":"string","enum":["OBJECT"]},"nullable":{"type":"boolean"}},"required":["type"]})
    );
}

#[test]
fn invalid_native_role_and_base64_padding_are_rejected() {
    assert!(
        protocol::request(
            &json!({"contents":[{"role":7,"parts":[{"text":"x"}]}]}),
            Gemini,
            OpenAi,
            "test"
        )
        .is_err()
    );
    assert!(protocol::request(&json!({"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,a==="}}]}]}), OpenAi, Claude, "test").is_err());
}

#[test]
fn gemini_parallel_function_responses_share_one_turn_in_call_order() {
    let body = json!({"messages":[
        {"role":"assistant","tool_calls":[
            {"id":"a","type":"function","function":{"name":"first","arguments":"{}"}},
            {"id":"b","type":"function","function":{"name":"second","arguments":"{}"}}
        ]},
        {"role":"tool","tool_call_id":"b","content":"2"},
        {"role":"tool","tool_call_id":"a","content":"1"},
        {"role":"assistant","content":"Done"}
    ]});
    let converted = protocol::request(&body, OpenAi, Gemini, "test").unwrap();
    let contents = converted["contents"].as_array().unwrap();
    assert_eq!(contents.len(), 3);
    assert_eq!(contents[1]["role"], "user");
    assert_eq!(contents[1]["parts"].as_array().unwrap().len(), 2);
    assert_eq!(contents[1]["parts"][0]["functionResponse"]["id"], "a");
    assert_eq!(contents[1]["parts"][1]["functionResponse"]["id"], "b");
    let partial = json!({"messages":body["messages"].as_array().unwrap()[..2]});
    assert!(protocol::request(&partial, OpenAi, Gemini, "test").is_err());
}

#[test]
fn gemini_visible_signed_text_remains_visible_and_round_trips_its_signature() {
    let parts = json!([
        {"text":"Plan","thought":true,"thoughtSignature":"thinking-sig"},
        {"text":"The answer","thoughtSignature":"answer-sig"}
    ]);
    let native = json!({"responseId":"r","candidates":[{"index":0,"content":{"role":"model","parts":parts},"finishReason":"STOP"}]});
    let openai = protocol::response(&native, Gemini, OpenAi, "test").unwrap();
    let message = &openai["choices"][0]["message"];
    assert_eq!(message["content"], "The answer");
    assert_eq!(message["reasoning_content"], "Plan");
    assert_eq!(
        message["extra_content"]["google"]["text_parts"][0]["thoughtSignature"],
        "answer-sig"
    );
    let restored = protocol::response(&openai, OpenAi, Gemini, "test").unwrap();
    assert_eq!(restored["candidates"][0]["content"]["parts"], parts);
    let replay = protocol::request(&json!({"messages":[message]}), OpenAi, Gemini, "test").unwrap();
    assert_eq!(replay["contents"][0]["parts"], parts);
}

#[test]
fn claude_matched_stop_sequence_becomes_stop_and_retains_match_metadata() {
    let native = json!({"id":"msg_s","type":"message","role":"assistant","model":"claude-test","content":[{"type":"text","text":"answer"}],"stop_reason":"stop_sequence","stop_sequence":"END","usage":{"input_tokens":2,"output_tokens":1}});
    let openai = protocol::response(&native, Claude, OpenAi, "test").unwrap();
    assert_eq!(openai["choices"][0]["finish_reason"], "stop");
    assert_eq!(openai["extra_content"]["anthropic"]["stop_sequence"], "END");
    let restored = protocol::response(&openai, OpenAi, Claude, "test").unwrap();
    assert_eq!(restored["stop_reason"], "stop_sequence");
    assert_eq!(restored["stop_sequence"], "END");
    let gemini = protocol::response(&native, Claude, Gemini, "test").unwrap();
    assert_eq!(gemini["candidates"][0]["finishReason"], "STOP");
    assert_eq!(gemini["extra_content"]["anthropic"]["stop_sequence"], "END");
}

#[test]
fn gemini_response_accepts_and_retains_modality_usage_diagnostics() {
    let usage = json!({"promptTokenCount":10,"candidatesTokenCount":2,"totalTokenCount":12,
        "promptTokensDetails":[{"modality":"TEXT","tokenCount":6},{"modality":"IMAGE","tokenCount":4}],
        "candidatesTokensDetails":[{"modality":"TEXT","tokenCount":2}],
        "cacheTokensDetails":[{"modality":"TEXT","tokenCount":0}],"trafficType":"ON_DEMAND"});
    let native = json!({"responseId":"r","candidates":[{"content":{"role":"model","parts":[{"text":"answer"}]},"finishReason":"STOP"}],"usageMetadata":usage});
    let openai = protocol::response(&native, Gemini, OpenAi, "test").unwrap();
    assert_eq!(openai["usage"]["prompt_tokens"], 10);
    assert_eq!(openai["usage"]["completion_tokens"], 2);
    let restored = protocol::response(&openai, OpenAi, Gemini, "test").unwrap();
    assert_eq!(restored["usageMetadata"], usage);
}

#[test]
fn nullable_gemini_enum_keeps_null_outside_the_enum_constraint() {
    let native = json!({"contents":[],"tools":[{"functionDeclarations":[{"name":"f","parameters":{"type":"OBJECT","properties":{"choice":{"type":"STRING","enum":["A"],"nullable":true}}}}]}]});
    let openai = protocol::request(&native, Gemini, OpenAi, "test").unwrap();
    assert_eq!(
        openai["tools"][0]["function"]["parameters"]["properties"]["choice"],
        json!({"anyOf":[{"type":"string","enum":["A"]},{"type":"null"}]})
    );
}
