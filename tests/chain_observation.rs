use cross_network_market_maker::chain::parse_rpc_coin_amount;
use serde_json::Value;

fn json_number(value: &str) -> Value {
    serde_json::from_str(value).expect("valid JSON number")
}

#[test]
fn rpc_coin_amount_parses_exact_decimal_and_scientific_values() {
    let cases = [
        (json_number("1e-8"), 1),
        (json_number("3.3e-6"), 330),
        (json_number("1E+2"), 10_000_000_000),
        (Value::String("0.0000000100".to_owned()), 1),
        (Value::String("100e-10".to_owned()), 1),
        (
            Value::String("0e-999999999999999999999999999999".to_owned()),
            0,
        ),
        (Value::String("0.125".to_owned()), 12_500_000),
        (Value::String("184467440737.09551615".to_owned()), u64::MAX),
        (json_number("18446744073709551615e-8"), u64::MAX),
    ];

    cases.into_iter().for_each(|(value, expected)| {
        assert_eq!(
            parse_rpc_coin_amount(&value).expect("exact amount"),
            expected
        );
    });
}

#[test]
fn rpc_coin_amount_rejects_precision_sign_and_overflow_errors() {
    let invalid = [
        Value::String("0.000000001".to_owned()),
        Value::String("1e-9".to_owned()),
        Value::String("-1".to_owned()),
        Value::String("1.000000001".to_owned()),
        Value::String("1e".to_owned()),
        Value::String("1e+".to_owned()),
        Value::String("1e2e3".to_owned()),
        Value::String("+1".to_owned()),
        Value::String("".to_owned()),
        Value::String(".1".to_owned()),
        Value::String("1.2.3".to_owned()),
        Value::String("1.".to_owned()),
        Value::String("184467440737.09551616".to_owned()),
        json_number("1e999999999999999999999999999999"),
        Value::String("1e-999999999999999999999999999999".to_owned()),
        Value::String("1e999999999999999999999999999999999999999999".to_owned()),
        Value::Bool(true),
        Value::Null,
    ];

    invalid.into_iter().for_each(|value| {
        assert!(parse_rpc_coin_amount(&value).is_err());
    });
}
