#![feature(try_blocks)]

extern crate chrono;
extern crate futures;
extern crate msql_srv;
extern crate mysql_async;
extern crate mysql_common as myc;
extern crate nom;
extern crate tokio;

use mysql_async::prelude::*;
use std::io;
use std::thread;
use tokio::net::tcp::TcpStream;
use tokio::prelude::*;

use msql_srv::*;

macro_rules! do_and_finish {
    ($start:expr => |$w:ident| $more:block) => {{
        #[allow(unused_mut)]
        match $start {
            Ok(mut $w) => {
                #[warn(unused_mut)]
                let r = try { $more };
                match r {
                    Ok(w) => future::Either::A(w.finish()),
                    Err(e) => future::Either::B(future::err(e)),
                }
            }
            Err(e) => future::Either::B(future::err(e)),
        }
    }};
}

struct TestingShim<Q, P, E> {
    columns: Vec<Column>,
    params: Vec<Column>,
    on_q: Q,
    on_p: P,
    on_e: E,
}

impl<W, Q, QF, P, E, EF> Service<W> for TestingShim<Q, P, E>
where
    Q: FnMut(&str, QueryResultWriter<W, MissingService>) -> QF + 'static,
    QF: IntoFuture<Item = PartialServiceState<W, MissingService>, Error = io::Error> + 'static,
    P: FnMut(&str) -> u32 + 'static,
    E: FnMut(u32, Vec<ParamValue>, QueryResultWriter<W, MissingParams>) -> EF + 'static,
    EF: IntoFuture<Item = PartialServiceState<W, MissingParams>, Error = io::Error> + 'static,
    W: AsyncWrite + 'static,
{
    type Error = io::Error;
    type ResponseFut = Box<Future<Item = ServiceState<W, Self>, Error = Self::Error>>;

    fn on_request(mut self, r: Request<W>) -> Self::ResponseFut {
        match r {
            Request::Prepare { query, info } => {
                let id = (self.on_p)(query);
                Box::new(
                    info.reply(id, &self.params, &self.columns)
                        .map(move |p| p.finish(self)),
                )
            }
            Request::Execute {
                id,
                mut params,
                results,
            } => {
                let mut ps = Vec::new();
                while let Some(p) = params.next() {
                    ps.push(p);
                }
                Box::new(
                    (self.on_e)(id, ps, results)
                        .into_future()
                        .map(move |p| p.add(params).finish(self)),
                )
            }
            Request::Close { rest, .. } => Box::new(futures::future::ok(rest.finish(self))),
            Request::Query { query, results } => Box::new(
                (self.on_q)(query, results)
                    .into_future()
                    .map(move |p| p.finish(self)),
            ),
        }
    }
}

type WH = tokio::io::WriteHalf<TcpStream>;

impl<Q, QF, P, E, EF> TestingShim<Q, P, E>
where
    Q: FnMut(&str, QueryResultWriter<WH, MissingService>) -> QF + Send + 'static,
    P: FnMut(&str) -> u32 + Send + 'static,
    E: FnMut(u32, Vec<ParamValue>, QueryResultWriter<WH, MissingParams>) -> EF + Send + 'static,
{
    fn new(on_q: Q, on_p: P, on_e: E) -> Self {
        TestingShim {
            columns: Vec::new(),
            params: Vec::new(),
            on_q,
            on_p,
            on_e,
        }
    }

    fn with_params(mut self, p: Vec<Column>) -> Self {
        self.params = p;
        self
    }

    fn with_columns(mut self, c: Vec<Column>) -> Self {
        self.columns = c;
        self
    }

    fn test<C, F>(self, c: C)
    where
        QF: IntoFuture<Item = PartialServiceState<WH, MissingService>, Error = io::Error> + 'static,
        EF: IntoFuture<Item = PartialServiceState<WH, MissingParams>, Error = io::Error> + 'static,
        F: IntoFuture<Item = (), Error = mysql_async::errors::Error>,
        C: FnOnce(mysql_async::Conn) -> F,
    {
        let listener = ::std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let jh = thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            tokio::runtime::current_thread::block_on_all(future::lazy(move || {
                let s = TcpStream::from_std(s, &tokio::reactor::Handle::default()).unwrap();
                msql_srv::single(self, s)
            }))
        });

        tokio::runtime::current_thread::block_on_all(
            mysql_async::Conn::new(&format!("mysql://127.0.0.1:{}", port))
                .and_then(move |db| c(db)),
        )
        .unwrap();
        jh.join().unwrap().unwrap();
    }
}

#[test]
fn it_connects() {
    TestingShim::new(
        |_, _| {
            future::err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "got unexpected query",
            ))
        },
        |_| unreachable!(),
        |_, _, _| {
            future::err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "got unexpected execute",
            ))
        },
    )
    .test(|_| Ok(()))
}

#[test]
fn it_pings() {
    TestingShim::new(
        |_, _| {
            future::err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "got unexpected query",
            ))
        },
        |_| unreachable!(),
        |_, _, _| {
            future::err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "got unexpected execute",
            ))
        },
    )
    .test(|db| db.ping().map(|_| ()))
}

#[test]
fn empty_response() {
    TestingShim::new(
        |_, w| w.completed(0, 0),
        |_| unreachable!(),
        |_, _, _| {
            future::err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "got unexpected execute",
            ))
        },
    )
    .test(|db| {
        db.query("SELECT a, b FROM foo")
            .and_then(|r| r.collect::<mysql_async::Row>())
            .and_then(|(_, rs)| {
                assert_eq!(rs.len(), 0);
                Ok(())
            })
    })
}

#[test]
fn no_rows() {
    let cols = [Column {
        table: String::new(),
        column: "a".to_owned(),
        coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
        colflags: myc::constants::ColumnFlags::empty(),
    }];
    TestingShim::new(
        move |_, w| do_and_finish!(w.start(&cols[..]) => |w| { w }),
        |_| unreachable!(),
        |_, _, _| {
            future::err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "got unexpected execute",
            ))
        },
    )
    .test(|db| {
        db.query("SELECT a, b FROM foo")
            .and_then(|r| r.collect::<mysql_async::Row>())
            .and_then(|(_, rs)| {
                assert_eq!(rs.len(), 0);
                Ok(())
            })
    })
}

#[test]
fn no_columns() {
    TestingShim::new(
        move |_, w| do_and_finish!(w.start(&[]) => |w| { w }),
        |_| unreachable!(),
        |_, _, _| {
            future::err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "got unexpected execute",
            ))
        },
    )
    .test(|db| {
        db.query("SELECT a, b FROM foo")
            .and_then(|r| r.collect::<mysql_async::Row>())
            .and_then(|(_, rs)| {
                assert_eq!(rs.len(), 0);
                Ok(())
            })
    })
}

#[test]
fn no_columns_but_rows() {
    TestingShim::new(
        move |_, w| {
            do_and_finish!(w.start(&[]) => |w| {
                w.write_col(42)?;
                w
            })
        },
        |_| unreachable!(),
        |_, _, _| {
            future::err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "got unexpected execute",
            ))
        },
    )
    .test(|db| {
        db.query("SELECT a, b FROM foo")
            .and_then(|r| r.collect::<mysql_async::Row>())
            .and_then(|(_, rs)| {
                assert_eq!(rs.len(), 0);
                Ok(())
            })
    })
}

#[test]
fn error_response() {
    let err = (ErrorKind::ER_NO, "clearly not");
    TestingShim::new(
        move |_, w| w.error(err.0, err.1.as_bytes()),
        |_| unreachable!(),
        |_, _, _| {
            future::err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "got unexpected execute",
            ))
        },
    )
    .test(|db| {
        db.query("SELECT a, b FROM foo").then(|r| {
            match r {
                Ok(_) => assert!(false),
                Err(mysql_async::errors::Error(
                    mysql_async::errors::ErrorKind::Server(ref state, code, ref msg),
                    _,
                )) => {
                    assert_eq!(
                        state,
                        &String::from_utf8(err.0.sqlstate().to_vec()).unwrap()
                    );
                    assert_eq!(code, err.0 as u16);
                    assert_eq!(msg, &err.1);
                }
                Err(e) => {
                    eprintln!("unexpected {:?}", e);
                    assert!(false);
                }
            }
            Ok(())
        })
    })
}

#[test]
fn empty_on_drop() {
    let cols = [Column {
        table: String::new(),
        column: "a".to_owned(),
        coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
        colflags: myc::constants::ColumnFlags::empty(),
    }];
    TestingShim::new(
        move |_, w| do_and_finish!(w.start(&cols[..]) => |w| { w }),
        |_| unreachable!(),
        |_, _, _| {
            future::err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "got unexpected execute",
            ))
        },
    )
    .test(|db| {
        db.query("SELECT a, b FROM foo")
            .and_then(|r| r.collect::<mysql_async::Row>())
            .and_then(|(_, rs)| {
                assert_eq!(rs.len(), 0);
                Ok(())
            })
    })
}

#[test]
fn it_queries_nulls() {
    TestingShim::new(
        |_, w| {
            let cols = &[Column {
                table: String::new(),
                column: "a".to_owned(),
                coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
                colflags: myc::constants::ColumnFlags::empty(),
            }];

            do_and_finish!(w.start(&cols[..]) => |w| {
                w.write_col(None::<i16>)?;
                w
            })
        },
        |_| unreachable!(),
        |_, _, _| {
            future::err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "got unexpected execute",
            ))
        },
    )
    .test(|db| {
        db.query("SELECT a, b FROM foo")
            .and_then(|r| r.collect::<mysql_async::Row>())
            .and_then(|(_, rs)| {
                assert_eq!(rs.len(), 1);
                assert_eq!(rs[0].len(), 1);
                assert_eq!(rs[0][0], mysql_async::Value::NULL);
                Ok(())
            })
    })
}

#[test]
fn it_queries() {
    TestingShim::new(
        |_, w| {
            let cols = &[Column {
                table: String::new(),
                column: "a".to_owned(),
                coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
                colflags: myc::constants::ColumnFlags::empty(),
            }];

            do_and_finish!(w.start(cols) => |w| {
                w.write_col(1024i16)?;
                w
            })
        },
        |_| unreachable!(),
        |_, _, _| {
            future::err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "got unexpected execute",
            ))
        },
    )
    .test(|db| {
        db.query("SELECT a, b FROM foo")
            .and_then(|r| r.collect::<mysql_async::Row>())
            .and_then(|(_, rs)| {
                assert_eq!(rs.len(), 1);
                assert_eq!(rs[0].len(), 1);
                assert_eq!(rs[0].get::<i16, _>(0), Some(1024));
                Ok(())
            })
    })
}

#[test]
fn it_queries_many_rows() {
    TestingShim::new(
        |_, w| {
            let cols = &[
                Column {
                    table: String::new(),
                    column: "a".to_owned(),
                    coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
                    colflags: myc::constants::ColumnFlags::empty(),
                },
                Column {
                    table: String::new(),
                    column: "b".to_owned(),
                    coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
                    colflags: myc::constants::ColumnFlags::empty(),
                },
            ];

            do_and_finish!(w.start(cols) => |w| {
                w.write_col(1024i16)?;
                w.write_col(1025i16)?;
                w.end_row()?;
                w.write_row(&[1024i16, 1025i16])?;
                w
            })
        },
        |_| unreachable!(),
        |_, _, _| {
            future::err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "got unexpected execute",
            ))
        },
    )
    .test(|db| {
        db.query("SELECT a, b FROM foo")
            .and_then(|r| r.collect::<mysql_async::Row>())
            .and_then(|(_, rs)| {
                assert_eq!(rs.len(), 2);
                assert_eq!(rs[0].len(), 2);
                assert_eq!(rs[0].get::<i16, _>(0), Some(1024));
                assert_eq!(rs[0].get::<i16, _>(1), Some(1025));
                assert_eq!(rs[1].len(), 2);
                assert_eq!(rs[1].get::<i16, _>(0), Some(1024));
                assert_eq!(rs[1].get::<i16, _>(1), Some(1025));
                Ok(())
            })
    })
}

#[test]
fn it_prepares() {
    let cols = vec![Column {
        table: String::new(),
        column: "a".to_owned(),
        coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
        colflags: myc::constants::ColumnFlags::empty(),
    }];
    let cols2 = cols.clone();
    let params = vec![Column {
        table: String::new(),
        column: "c".to_owned(),
        coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
        colflags: myc::constants::ColumnFlags::empty(),
    }];

    TestingShim::new(
        |_, _| {
            future::err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "got unexpected query",
            ))
        },
        |q| {
            assert_eq!(q, "SELECT a FROM b WHERE c = ?");
            41
        },
        move |stmt, params, w| {
            assert_eq!(stmt, 41);
            assert_eq!(params.len(), 1);
            // rust-mysql sends all numbers as LONGLONG
            assert_eq!(
                params[0].coltype,
                myc::constants::ColumnType::MYSQL_TYPE_LONGLONG
            );
            assert_eq!(Into::<i8>::into(params[0].value), 42i8);

            do_and_finish!(w.start(&cols) => |w| {
                w.write_col(1024i16)?;
                w
            })
        },
    )
    .with_params(params)
    .with_columns(cols2)
    .test(|db| {
        db.prep_exec("SELECT a FROM b WHERE c = ?", (42i16,))
            .and_then(|r| r.collect::<mysql_async::Row>())
            .and_then(|(_, rs)| {
                assert_eq!(rs.len(), 1);
                assert_eq!(rs[0].len(), 1);
                assert_eq!(rs[0].get::<i16, _>(0), Some(1024));
                Ok(())
            })
    })
}

#[test]
fn insert_exec() {
    let params = vec![
        Column {
            table: String::new(),
            column: "username".to_owned(),
            coltype: myc::constants::ColumnType::MYSQL_TYPE_VARCHAR,
            colflags: myc::constants::ColumnFlags::empty(),
        },
        Column {
            table: String::new(),
            column: "email".to_owned(),
            coltype: myc::constants::ColumnType::MYSQL_TYPE_VARCHAR,
            colflags: myc::constants::ColumnFlags::empty(),
        },
        Column {
            table: String::new(),
            column: "pw".to_owned(),
            coltype: myc::constants::ColumnType::MYSQL_TYPE_VARCHAR,
            colflags: myc::constants::ColumnFlags::empty(),
        },
        Column {
            table: String::new(),
            column: "created".to_owned(),
            coltype: myc::constants::ColumnType::MYSQL_TYPE_DATETIME,
            colflags: myc::constants::ColumnFlags::empty(),
        },
        Column {
            table: String::new(),
            column: "session".to_owned(),
            coltype: myc::constants::ColumnType::MYSQL_TYPE_VARCHAR,
            colflags: myc::constants::ColumnFlags::empty(),
        },
        Column {
            table: String::new(),
            column: "rss".to_owned(),
            coltype: myc::constants::ColumnType::MYSQL_TYPE_VARCHAR,
            colflags: myc::constants::ColumnFlags::empty(),
        },
        Column {
            table: String::new(),
            column: "mail".to_owned(),
            coltype: myc::constants::ColumnType::MYSQL_TYPE_VARCHAR,
            colflags: myc::constants::ColumnFlags::empty(),
        },
    ];

    TestingShim::new(
        |_, _| {
            future::err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "got unexpected query",
            ))
        },
        |_| 1,
        move |_, params, w| {
            assert_eq!(params.len(), 7);
            assert_eq!(
                params[0].coltype,
                myc::constants::ColumnType::MYSQL_TYPE_VAR_STRING
            );
            assert_eq!(
                params[1].coltype,
                myc::constants::ColumnType::MYSQL_TYPE_VAR_STRING
            );
            assert_eq!(
                params[2].coltype,
                myc::constants::ColumnType::MYSQL_TYPE_VAR_STRING
            );
            assert_eq!(
                params[3].coltype,
                myc::constants::ColumnType::MYSQL_TYPE_DATETIME
            );
            assert_eq!(
                params[4].coltype,
                myc::constants::ColumnType::MYSQL_TYPE_VAR_STRING
            );
            assert_eq!(
                params[5].coltype,
                myc::constants::ColumnType::MYSQL_TYPE_VAR_STRING
            );
            assert_eq!(
                params[6].coltype,
                myc::constants::ColumnType::MYSQL_TYPE_VAR_STRING
            );
            assert_eq!(Into::<&str>::into(params[0].value), "user199");
            assert_eq!(Into::<&str>::into(params[1].value), "user199@example.com");
            assert_eq!(
                Into::<&str>::into(params[2].value),
                "$2a$10$Tq3wrGeC0xtgzuxqOlc3v.07VTUvxvwI70kuoVihoO2cE5qj7ooka"
            );
            assert_eq!(
                Into::<chrono::NaiveDateTime>::into(params[3].value),
                chrono::NaiveDate::from_ymd(2018, 4, 6).and_hms(13, 0, 56)
            );
            assert_eq!(Into::<&str>::into(params[4].value), "token199");
            assert_eq!(Into::<&str>::into(params[5].value), "rsstoken199");
            assert_eq!(Into::<&str>::into(params[6].value), "mtok199");

            w.completed(42, 1)
        },
    )
    .with_params(params)
    .test(|db| {
        db.prep_exec(
            "INSERT INTO `users` \
             (`username`, `email`, `password_digest`, `created_at`, \
             `session_token`, `rss_token`, `mailing_list_token`) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            (
                "user199",
                "user199@example.com",
                "$2a$10$Tq3wrGeC0xtgzuxqOlc3v.07VTUvxvwI70kuoVihoO2cE5qj7ooka",
                mysql_async::Value::Date(2018, 4, 6, 13, 0, 56, 0),
                "token199",
                "rsstoken199",
                "mtok199",
            ),
        )
        .and_then(|res| {
            assert_eq!(res.affected_rows(), 42);
            assert_eq!(res.last_insert_id(), Some(1));
            Ok(())
        })
    })
}

#[test]
fn send_long() {
    let cols = vec![Column {
        table: String::new(),
        column: "a".to_owned(),
        coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
        colflags: myc::constants::ColumnFlags::empty(),
    }];
    let cols2 = cols.clone();
    let params = vec![Column {
        table: String::new(),
        column: "c".to_owned(),
        coltype: myc::constants::ColumnType::MYSQL_TYPE_BLOB,
        colflags: myc::constants::ColumnFlags::empty(),
    }];

    TestingShim::new(
        |_, _| {
            future::err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "got unexpected query",
            ))
        },
        |q| {
            assert_eq!(q, "SELECT a FROM b WHERE c = ?");
            41
        },
        move |stmt, params, w| {
            assert_eq!(stmt, 41);
            assert_eq!(params.len(), 1);
            // rust-mysql sends all strings as VAR_STRING
            assert_eq!(
                params[0].coltype,
                myc::constants::ColumnType::MYSQL_TYPE_VAR_STRING
            );
            assert_eq!(Into::<&[u8]>::into(params[0].value), b"Hello world");

            do_and_finish!(w.start(&cols) => |w| {
                w.write_col(1024i16)?;
                w
            })
        },
    )
    .with_params(params)
    .with_columns(cols2)
    .test(|db| {
        db.prep_exec("SELECT a FROM b WHERE c = ?", (b"Hello world",))
            .and_then(|r| r.collect::<mysql_async::Row>())
            .and_then(|(_, rs)| {
                assert_eq!(rs.len(), 1);
                assert_eq!(rs[0].len(), 1);
                assert_eq!(rs[0].get::<i16, _>(0), Some(1024));
                Ok(())
            })
    })
}

#[test]
fn it_prepares_many() {
    let cols = vec![
        Column {
            table: String::new(),
            column: "a".to_owned(),
            coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
            colflags: myc::constants::ColumnFlags::empty(),
        },
        Column {
            table: String::new(),
            column: "b".to_owned(),
            coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
            colflags: myc::constants::ColumnFlags::empty(),
        },
    ];
    let cols2 = cols.clone();

    TestingShim::new(
        |_, _| {
            future::err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "got unexpected query",
            ))
        },
        |q| {
            assert_eq!(q, "SELECT a, b FROM x");
            41
        },
        move |stmt, params, w| {
            assert_eq!(stmt, 41);
            assert_eq!(params.len(), 0);

            do_and_finish!(w.start(&cols) => |w| {
                w.write_col(1024i16)?;
                w.write_col(1025i16)?;
                w.end_row()?;
                w.write_row(&[1024i16, 1025i16])?;
                w
            })
        },
    )
    .with_params(Vec::new())
    .with_columns(cols2)
    .test(|db| {
        db.prep_exec("SELECT a, b FROM x", ())
            .and_then(|r| r.collect::<mysql_async::Row>())
            .and_then(|(_, rs)| {
                assert_eq!(rs.len(), 2);
                assert_eq!(rs[0].len(), 2);
                assert_eq!(rs[0].get::<i16, _>(0), Some(1024));
                assert_eq!(rs[0].get::<i16, _>(1), Some(1025));
                assert_eq!(rs[1].len(), 2);
                assert_eq!(rs[1].get::<i16, _>(0), Some(1024));
                assert_eq!(rs[1].get::<i16, _>(1), Some(1025));
                Ok(())
            })
    })
}

#[test]
fn prepared_empty() {
    let cols = vec![Column {
        table: String::new(),
        column: "a".to_owned(),
        coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
        colflags: myc::constants::ColumnFlags::empty(),
    }];
    let cols2 = cols.clone();
    let params = vec![Column {
        table: String::new(),
        column: "c".to_owned(),
        coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
        colflags: myc::constants::ColumnFlags::empty(),
    }];

    TestingShim::new(
        |_, _| {
            future::err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "got unexpected query",
            ))
        },
        |_| 0,
        move |_, params, w| {
            assert!(!params.is_empty());
            w.completed(0, 0)
        },
    )
    .with_params(params)
    .with_columns(cols2)
    .test(|db| {
        db.prep_exec("SELECT a FROM b WHERE c = ?", (42i16,))
            .and_then(|r| r.collect::<mysql_async::Row>())
            .and_then(|(_, rs)| {
                assert_eq!(rs.len(), 0);
                Ok(())
            })
    })
}

#[test]
fn prepared_no_params() {
    let cols = vec![Column {
        table: String::new(),
        column: "a".to_owned(),
        coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
        colflags: myc::constants::ColumnFlags::empty(),
    }];
    let cols2 = cols.clone();
    let params = vec![];

    TestingShim::new(
        |_, _| {
            future::err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "got unexpected query",
            ))
        },
        |_| 0,
        move |_, params, w| {
            assert!(params.is_empty());
            do_and_finish!(w.start(&cols) => |w| {
                w.write_col(1024i16)?;
                w
            })
        },
    )
    .with_params(params)
    .with_columns(cols2)
    .test(|db| {
        db.prep_exec("foo", ())
            .and_then(|r| r.collect::<mysql_async::Row>())
            .and_then(|(_, rs)| {
                assert_eq!(rs.len(), 1);
                assert_eq!(rs[0].len(), 1);
                assert_eq!(rs[0].get::<i16, _>(0), Some(1024));
                Ok(())
            })
    })
}

#[test]
fn prepared_nulls() {
    let cols = vec![
        Column {
            table: String::new(),
            column: "a".to_owned(),
            coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
            colflags: myc::constants::ColumnFlags::empty(),
        },
        Column {
            table: String::new(),
            column: "b".to_owned(),
            coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
            colflags: myc::constants::ColumnFlags::empty(),
        },
    ];
    let cols2 = cols.clone();
    let params = vec![
        Column {
            table: String::new(),
            column: "c".to_owned(),
            coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
            colflags: myc::constants::ColumnFlags::empty(),
        },
        Column {
            table: String::new(),
            column: "d".to_owned(),
            coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
            colflags: myc::constants::ColumnFlags::empty(),
        },
    ];

    TestingShim::new(
        |_, _| {
            future::err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "got unexpected query",
            ))
        },
        |_| 0,
        move |_, params, w| {
            assert_eq!(params.len(), 2);
            assert!(params[0].value.is_null());
            assert!(!params[1].value.is_null());
            assert_eq!(
                params[0].coltype,
                myc::constants::ColumnType::MYSQL_TYPE_SHORT
            );
            // rust-mysql sends all numbers as LONGLONG :'(
            assert_eq!(
                params[1].coltype,
                myc::constants::ColumnType::MYSQL_TYPE_LONGLONG
            );
            assert_eq!(Into::<i8>::into(params[1].value), 42i8);

            do_and_finish!(w.start(&cols) => |w| {
                w.write_row(vec![None::<i16>, Some(42)])?;
                w
            })
        },
    )
    .with_params(params)
    .with_columns(cols2)
    .test(|db| {
        db.prep_exec(
            "SELECT a, b FROM x WHERE c = ? AND d = ?",
            (mysql_async::Value::NULL, 42),
        )
        .and_then(|r| r.collect::<mysql_async::Row>())
        .and_then(|(_, rs)| {
            assert_eq!(rs.len(), 1);
            assert_eq!(rs[0].len(), 2);
            assert_eq!(rs[0].get::<Option<i16>, _>(0), Some(None));
            assert_eq!(rs[0].get::<i16, _>(1), Some(42));
            Ok(())
        })
    })
}

#[test]
fn prepared_no_rows() {
    let cols = vec![Column {
        table: String::new(),
        column: "a".to_owned(),
        coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
        colflags: myc::constants::ColumnFlags::empty(),
    }];
    let cols2 = cols.clone();
    TestingShim::new(
        |_, _| {
            future::err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "got unexpected query",
            ))
        },
        |_| 0,
        move |_, _, w| do_and_finish!(w.start(&cols[..]) => |w| { w }),
    )
    .with_columns(cols2)
    .test(|db| {
        db.prep_exec("SELECT a, b FROM foo", ())
            .and_then(|r| r.collect::<mysql_async::Row>())
            .inspect(|(_, rs)| assert_eq!(rs.len(), 0))
            .map(|_| ())
    })
}

#[test]
fn prepared_no_cols_but_rows() {
    TestingShim::new(
        |_, _| {
            future::err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "got unexpected query",
            ))
        },
        |_| 0,
        move |_, _, w| {
            do_and_finish!(w.start(&[]) => |w| {
                w.write_col(42)?;
                w
            })
        },
    )
    .test(|db| {
        db.prep_exec("SELECT a, b FROM foo", ())
            .and_then(|r| r.collect::<mysql_async::Row>())
            .inspect(|(_, rs)| assert_eq!(rs.len(), 0))
            .map(|_| ())
    })
}

#[test]
fn prepared_no_cols() {
    TestingShim::new(
        |_, _| {
            future::err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "got unexpected query",
            ))
        },
        |_| 0,
        move |_, _, w| do_and_finish!(w.start(&[]) => |w| { w }),
    )
    .test(|db| {
        db.prep_exec("SELECT a, b FROM foo", ())
            .and_then(|r| r.collect::<mysql_async::Row>())
            .inspect(|(_, rs)| assert_eq!(rs.len(), 0))
            .map(|_| ())
    })
}
