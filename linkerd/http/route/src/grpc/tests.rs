use super::{r#match::*, *};
use crate::http::MatchHeader;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Policy {
    Expected,
    Unexpected,
}

impl Default for Policy {
    fn default() -> Self {
        Self::Unexpected
    }
}

#[test]
fn default() {
    let rts = vec![Route {
        hosts: vec![],
        rules: vec![Rule {
            matches: vec![MatchRoute::default()],
            policy: Policy::Expected,
        }],
    }];

    let req = http::Request::builder()
        .method(http::Method::POST)
        .uri("http://foo.example.com/foo/bar")
        .body(())
        .unwrap();
    let (_, policy) = find(&rts, &req).expect("must match");
    assert_eq!(*policy, Policy::Expected, "incorrect rule matched");
}

/// Given two equivalent routes, choose the explicit hostname match and not
/// the wildcard.
#[test]
fn hostname_precedence() {
    let rts = vec![
        Route {
            hosts: vec!["*.example.com".parse().unwrap()],
            rules: vec![Rule {
                matches: vec![MatchRoute {
                    rpc: MatchRpc {
                        service: Some("foo".to_string()),
                        method: Some("bar".to_string()),
                    },
                    ..MatchRoute::default()
                }],
                ..Rule::default()
            }],
        },
        Route {
            hosts: vec!["foo.example.com".parse().unwrap()],
            rules: vec![Rule {
                matches: vec![MatchRoute {
                    rpc: MatchRpc {
                        service: Some("foo".to_string()),
                        method: Some("bar".to_string()),
                    },
                    ..MatchRoute::default()
                }],
                policy: Policy::Expected,
            }],
        },
    ];

    let req = http::Request::builder()
        .method(http::Method::POST)
        .uri("http://foo.example.com/foo/bar")
        .body(())
        .unwrap();
    let (_, policy) = find(&rts, &req).expect("must match");
    assert_eq!(*policy, Policy::Expected, "incorrect rule matched");
}

/// Given two equivalent routes, choose the longer path match.
#[test]
fn method_precedence() {
    let rts = vec![
        Route {
            rules: vec![Rule {
                matches: vec![MatchRoute {
                    rpc: MatchRpc {
                        service: Some("foo".to_string()),
                        method: None,
                    },
                    ..MatchRoute::default()
                }],
                ..Rule::default()
            }],
            hosts: vec![],
        },
        Route {
            rules: vec![Rule {
                matches: vec![MatchRoute {
                    rpc: MatchRpc {
                        service: Some("foo".to_string()),
                        method: Some("bar".to_string()),
                    },
                    ..MatchRoute::default()
                }],
                policy: Policy::Expected,
            }],
            hosts: vec![],
        },
    ];

    let req = http::Request::builder()
        .method(http::Method::POST)
        .uri("http://foo.example.com/foo/bar")
        .body(())
        .unwrap();
    let (_, policy) = find(&rts, &req).expect("must match");
    assert_eq!(*policy, Policy::Expected, "incorrect rule matched");
}

/// Given two routes with header matches, use the one that matches more
/// headers.
#[test]
fn header_count_precedence() {
    let rts = vec![
        Route {
            rules: vec![Rule {
                matches: vec![MatchRoute {
                    headers: vec![
                        MatchHeader::Exact("x-foo".parse().unwrap(), "bar".parse().unwrap()),
                        MatchHeader::Exact("x-baz".parse().unwrap(), "qux".parse().unwrap()),
                    ],
                    ..MatchRoute::default()
                }],
                ..Rule::default()
            }],
            hosts: vec![],
        },
        Route {
            rules: vec![Rule {
                matches: vec![MatchRoute {
                    headers: vec![
                        MatchHeader::Exact("x-foo".parse().unwrap(), "bar".parse().unwrap()),
                        MatchHeader::Exact("x-baz".parse().unwrap(), "qux".parse().unwrap()),
                        MatchHeader::Exact("x-biz".parse().unwrap(), "qyx".parse().unwrap()),
                    ],
                    ..MatchRoute::default()
                }],
                policy: Policy::Expected,
            }],
            hosts: vec![],
        },
    ];

    let req = http::Request::builder()
        .method(http::Method::POST)
        .uri("http://www.example.com/foo/bar")
        .header("x-foo", "bar")
        .header("x-baz", "qux")
        .header("x-biz", "qyx")
        .body(())
        .unwrap();
    let (_, policy) = find(&rts, &req).expect("must match");
    assert_eq!(*policy, Policy::Expected, "incorrect rule matched");
}

/// Given two routes with header matches, use the one that matches more
/// headers.
#[test]
fn first_identical_wins() {
    let rts = vec![
        Route {
            rules: vec![
                Rule {
                    policy: Policy::Expected,
                    ..Rule::default()
                },
                // Redundant rule.
                Rule::default(),
            ],
            hosts: vec![],
        },
        // Redundant route.
        Route {
            rules: vec![Rule::default()],
            hosts: vec![],
        },
    ];

    let req = http::Request::builder().body(()).unwrap();
    let (_, policy) = find(&rts, &req).expect("must match");
    assert_eq!(*policy, Policy::Expected, "incorrect rule matched");
}
