//! PDO-shaped storage surface (issue #15 spike): `PDO` + `PDOStatement`
//! backed by SQLite via rusqlite. Adapter-shaped: the API follows PDO,
//! internals can grow other drivers later.

use crate::error::PhpError;
use crate::interp::{CallArgs, Interp};
use crate::value::{ArrKey, ObjectInternal, PhpArray, PhpObject, Value};
use rusqlite::types::Value as SqlVal;
use rusqlite::Connection;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

fn a(args: &CallArgs, i: usize) -> Value {
    args.cells
        .get(i)
        .map(|c| c.borrow().clone())
        .unwrap_or(Value::Null)
}

fn conn_of(obj: &Rc<RefCell<PhpObject>>) -> Result<Rc<RefCell<Connection>>, PhpError> {
    match &obj.borrow().internal {
        Some(ObjectInternal::Sqlite { conn }) => Ok(conn.clone()),
        Some(ObjectInternal::SqliteStmt { conn, .. }) => Ok(conn.clone()),
        _ => Err(PhpError::fatal("PDO object not initialized".to_string(), 0)),
    }
}

fn to_sql(v: &Value) -> SqlVal {
    match v {
        Value::Null => SqlVal::Null,
        Value::Bool(b) => SqlVal::Integer(*b as i64),
        Value::Int(i) => SqlVal::Integer(*i),
        Value::Float(f) => SqlVal::Real(*f),
        other => SqlVal::Text(other.to_php_string()),
    }
}

fn from_sql(v: SqlVal) -> Value {
    match v {
        SqlVal::Null => Value::Null,
        SqlVal::Integer(i) => Value::Int(i),
        SqlVal::Real(f) => Value::Float(f),
        SqlVal::Text(t) => Value::str(t),
        SqlVal::Blob(b) => Value::str(String::from_utf8_lossy(&b).into_owned()),
    }
}

/// (rows, affected): rows carry (col_name, Value) per cell; non-SELECT
/// statements return an empty row set and the change count.
type Rows = Vec<Vec<(String, Value)>>;

fn run(
    conn: &Connection,
    sql: &str,
    params: &[Value],
    named: &HashMap<String, Value>,
) -> Result<(Rows, i64), String> {
    let mut st = conn.prepare(sql).map_err(|e| e.to_string())?;
    let cols: Vec<String> = st.column_names().iter().map(|s| s.to_string()).collect();
    if cols.is_empty() {
        let n = exec_named_or_pos(&mut st, params, named)?;
        return Ok((Vec::new(), n as i64));
    }
    let ncol = cols.len();
    let mut collected = Vec::new();
    {
        let mut r = if named.is_empty() {
            let vs: Vec<SqlVal> = params.iter().map(to_sql).collect();
            st.query(rusqlite::params_from_iter(vs))
        } else {
            let pairs = named_pairs(named);
            st.query(&pairs[..])
        }
        .map_err(|e| e.to_string())?;
        while let Some(row) = r.next().map_err(|e| e.to_string())? {
            let mut cells = Vec::with_capacity(ncol);
            for (i, c) in cols.iter().enumerate() {
                let v: SqlVal = row.get(i).unwrap_or(SqlVal::Null);
                cells.push((c.clone(), from_sql(v)));
            }
            collected.push(cells);
        }
    }
    // sqlite PDO reports 0 for rowCount() on SELECTs.
    Ok((collected, 0))
}

fn named_pairs(named: &HashMap<String, Value>) -> Vec<(&str, SqlVal)> {
    named.iter().map(|(k, v)| (k.as_str(), to_sql(v))).collect()
}

fn exec_named_or_pos(
    st: &mut rusqlite::Statement<'_>,
    params: &[Value],
    named: &HashMap<String, Value>,
) -> Result<usize, String> {
    if named.is_empty() {
        let vs: Vec<SqlVal> = params.iter().map(to_sql).collect();
        st.execute(rusqlite::params_from_iter(vs))
            .map_err(|e| e.to_string())
    } else {
        let pairs = named_pairs(named);
        st.execute(&pairs[..]).map_err(|e| e.to_string())
    }
}

fn stmt_rows(it: &mut Interp, rows: &[Vec<(String, Value)>], pos: usize, mode: i64) -> Value {
    let Some(row) = rows.get(pos) else {
        return Value::Bool(false);
    };
    match mode {
        3 => {
            // FETCH_NUM
            let mut arr = PhpArray::new();
            for (_, v) in row {
                arr.push(v.clone());
            }
            Value::Array(Rc::new(RefCell::new(arr)))
        }
        5 => {
            // FETCH_OBJ → stdClass
            let obj = it
                .instantiate_class("stdClass", vec![])
                .unwrap_or(Value::Null);
            if let Value::Object(o) = &obj {
                let mut ob = o.borrow_mut();
                for (k, v) in row {
                    ob.props.insert(k.clone(), Rc::new(RefCell::new(v.clone())));
                    ob.prop_order.push(k.clone());
                }
            }
            obj
        }
        2 => {
            // FETCH_ASSOC
            let mut arr = PhpArray::new();
            for (k, v) in row {
                arr.set(ArrKey::Str(k.clone().into()), v.clone());
            }
            Value::Array(Rc::new(RefCell::new(arr)))
        }
        _ => {
            // FETCH_BOTH (4): numeric then assoc
            let mut arr = PhpArray::new();
            for (i, (k, v)) in row.iter().enumerate() {
                arr.set(ArrKey::Int(i as i64), v.clone());
                arr.set(ArrKey::Str(k.clone().into()), v.clone());
            }
            Value::Array(Rc::new(RefCell::new(arr)))
        }
    }
}

pub fn pdo_method(
    it: &mut Interp,
    obj: &Rc<RefCell<PhpObject>>,
    name: &str,
    args: &CallArgs,
) -> Result<Option<Value>, PhpError> {
    match name.to_ascii_lowercase().as_str() {
        "__construct" => {
            let dsn = a(args, 0).to_php_string();
            let path = match dsn.strip_prefix("sqlite:") {
                Some(p) => p.to_string(),
                None => {
                    return Err(PhpError::uncaught(
                        "PDOException",
                        "could not find driver".to_string(),
                        it.cur_line,
                    ))
                }
            };
            let conn = if path == ":memory:" {
                Connection::open_in_memory()
            } else {
                Connection::open(&path)
            };
            match conn {
                Ok(c) => {
                    obj.borrow_mut().internal = Some(ObjectInternal::Sqlite {
                        conn: Rc::new(RefCell::new(c)),
                    });
                    Ok(Some(Value::Null))
                }
                Err(e) => Err(PhpError::uncaught(
                    "PDOException",
                    format!("SQLSTATE[HY000] [14] {}", e),
                    it.cur_line,
                )),
            }
        }
        "query" => {
            let sql = a(args, 0).to_php_string();
            let conn = conn_of(obj)?;
            let res = {
                let c = conn.borrow();
                run(&c, &sql, &[], &HashMap::new())
            };
            match res {
                Ok((rows, affected)) => {
                    let stmt = it.instantiate_class("PDOStatement", vec![])?;
                    if let Value::Object(o) = &stmt {
                        o.borrow_mut().internal = Some(ObjectInternal::SqliteStmt {
                            conn,
                            sql,
                            rows,
                            affected,
                            pos: 0,
                            bound: Vec::new(),
                            named: HashMap::new(),
                        });
                    }
                    Ok(Some(stmt))
                }
                Err(e) => {
                    it.warn_pub(&format!("PDO::query(): {}", e))?;
                    Ok(Some(Value::Bool(false)))
                }
            }
        }
        "exec" => {
            let sql = a(args, 0).to_php_string();
            let conn = conn_of(obj)?;
            let res = {
                let c = conn.borrow();
                c.execute_batch(&sql)
            };
            match res {
                Ok(()) => Ok(Some(Value::Int(conn.borrow().changes() as i64))),
                Err(e) => {
                    it.warn_pub(&format!("PDO::exec(): {}", e))?;
                    Ok(Some(Value::Bool(false)))
                }
            }
        }
        "prepare" => {
            let sql = a(args, 0).to_php_string();
            let conn = conn_of(obj)?;
            // SQLite validates at prepare time — surface syntax errors now
            let prep_err = {
                let c = conn.borrow();
                c.prepare(&sql).err()
            };
            if let Some(e) = prep_err {
                it.warn_pub(&format!(
                    "PDO::prepare(): SQLSTATE[HY000]: General error: {}",
                    e
                ))?;
                return Ok(Some(Value::Bool(false)));
            }
            let stmt = it.instantiate_class("PDOStatement", vec![])?;
            if let Value::Object(o) = &stmt {
                o.borrow_mut().internal = Some(ObjectInternal::SqliteStmt {
                    conn,
                    sql,
                    rows: Vec::new(),
                    affected: 0,
                    pos: 0,
                    bound: Vec::new(),
                    named: HashMap::new(),
                });
            }
            Ok(Some(stmt))
        }
        "lastinsertid" => {
            let conn = conn_of(obj)?;
            let n = {
                let c = conn.borrow();
                c.last_insert_rowid()
            };
            Ok(Some(Value::str(n.to_string())))
        }
        "begintransaction" => {
            let conn = conn_of(obj)?;
            let res = {
                let c = conn.borrow();
                c.execute_batch("BEGIN")
            };
            match res {
                Ok(()) => Ok(Some(Value::Bool(true))),
                Err(e) => {
                    it.warn_pub(&format!("PDO::beginTransaction(): {}", e))?;
                    Ok(Some(Value::Bool(false)))
                }
            }
        }
        "commit" => {
            let conn = conn_of(obj)?;
            let res = {
                let c = conn.borrow();
                c.execute_batch("COMMIT")
            };
            match res {
                Ok(()) => Ok(Some(Value::Bool(true))),
                Err(e) => {
                    it.warn_pub(&format!("PDO::commit(): {}", e))?;
                    Ok(Some(Value::Bool(false)))
                }
            }
        }
        "rollback" => {
            let conn = conn_of(obj)?;
            let res = {
                let c = conn.borrow();
                c.execute_batch("ROLLBACK")
            };
            match res {
                Ok(()) => Ok(Some(Value::Bool(true))),
                Err(e) => {
                    it.warn_pub(&format!("PDO::rollBack(): {}", e))?;
                    Ok(Some(Value::Bool(false)))
                }
            }
        }
        "intransaction" => {
            let conn = conn_of(obj)?;
            let ac = {
                let c = conn.borrow();
                c.is_autocommit()
            };
            Ok(Some(Value::Bool(!ac)))
        }
        "quote" => {
            let s = a(args, 0).to_php_string();
            Ok(Some(Value::str(format!("'{}'", s.replace('\'', "''")))))
        }
        "setattribute" => Ok(Some(Value::Bool(true))),
        "getattribute" => Ok(Some(Value::Null)),
        "errorcode" => Ok(Some(Value::str("00000"))),
        "errorinfo" => {
            let mut arr = PhpArray::new();
            arr.push(Value::str("00000"));
            arr.push(Value::Null);
            arr.push(Value::Null);
            Ok(Some(Value::Array(Rc::new(RefCell::new(arr)))))
        }
        _ => Ok(None),
    }
}

pub fn pdostmt_method(
    it: &mut Interp,
    obj: &Rc<RefCell<PhpObject>>,
    name: &str,
    args: &CallArgs,
) -> Result<Option<Value>, PhpError> {
    match name.to_ascii_lowercase().as_str() {
        "execute" => {
            let (params, named) = match a(args, 0) {
                Value::Array(arr) => {
                    let mut pos = Vec::new();
                    let mut named = HashMap::new();
                    for (k, c) in arr.borrow().iter() {
                        match k {
                            ArrKey::Str(s) => {
                                named.insert(
                                    format!(":{}", s.trim_start_matches(':')),
                                    c.borrow().clone(),
                                );
                            }
                            _ => pos.push(c.borrow().clone()),
                        }
                    }
                    (pos, named)
                }
                Value::Null => (Vec::new(), HashMap::new()),
                v => (vec![v], HashMap::new()),
            };
            let (conn, sql, bound, named_bound) = {
                let ob = obj.borrow();
                match &ob.internal {
                    Some(ObjectInternal::SqliteStmt {
                        conn,
                        sql,
                        bound,
                        named,
                        ..
                    }) => (conn.clone(), sql.clone(), bound.clone(), named.clone()),
                    _ => {
                        return Err(PhpError::fatal(
                            "PDOStatement object not initialized".to_string(),
                            0,
                        ))
                    }
                }
            };
            let params = if params.is_empty() { bound } else { params };
            let named = if named.is_empty() { named_bound } else { named };
            let res = {
                let c = conn.borrow();
                run(&c, &sql, &params, &named)
            };
            match res {
                Ok((rows, affected)) => {
                    if let Some(ObjectInternal::SqliteStmt {
                        rows: r,
                        affected: af,
                        pos,
                        ..
                    }) = &mut obj.borrow_mut().internal
                    {
                        *r = rows;
                        *af = affected;
                        *pos = 0;
                    }
                    Ok(Some(Value::Bool(true)))
                }
                Err(e) => {
                    it.warn_pub(&format!("PDOStatement::execute(): {}", e))?;
                    Ok(Some(Value::Bool(false)))
                }
            }
        }
        "fetch" | "fetchobject" => {
            let mode = if name == "fetchobject" {
                5
            } else {
                a(args, 0).to_int()
            };
            let take = {
                let mut ob = obj.borrow_mut();
                match &mut ob.internal {
                    Some(ObjectInternal::SqliteStmt { rows, pos, .. }) => {
                        let p = *pos;
                        if p < rows.len() {
                            *pos += 1;
                            Some(rows[p].clone())
                        } else {
                            None
                        }
                    }
                    _ => None,
                }
            };
            let Some(row) = take else {
                return Ok(Some(Value::Bool(false)));
            };
            let v = stmt_rows(it, &[row], 0, if mode <= 1 { 4 } else { mode });
            Ok(Some(v))
        }
        "fetchall" => {
            let mode = a(args, 0).to_int();
            let mode = if mode <= 1 { 4 } else { mode };
            let mut out = PhpArray::new();
            let take = {
                let mut ob = obj.borrow_mut();
                match &mut ob.internal {
                    Some(ObjectInternal::SqliteStmt { rows, pos, .. }) => {
                        let t = rows[*pos..].to_vec();
                        *pos = rows.len();
                        t
                    }
                    _ => Vec::new(),
                }
            };
            for row in &take {
                let wrap = vec![row.clone()];
                out.push(stmt_rows(it, &wrap, 0, mode));
            }
            Ok(Some(Value::Array(Rc::new(RefCell::new(out)))))
        }
        "fetchcolumn" => {
            let col = a(args, 0).to_int().max(0) as usize;
            let mut ob = obj.borrow_mut();
            if let Some(ObjectInternal::SqliteStmt { rows, pos, .. }) = &mut ob.internal {
                let p = *pos;
                if p >= rows.len() {
                    return Ok(Some(Value::Bool(false)));
                }
                *pos += 1;
                let v = rows[p]
                    .get(col)
                    .map(|(_, v)| v.clone())
                    .unwrap_or(Value::Bool(false));
                return Ok(Some(v));
            }
            Ok(Some(Value::Bool(false)))
        }
        "rowcount" => {
            let ob = obj.borrow();
            let n = match &ob.internal {
                Some(ObjectInternal::SqliteStmt { affected, .. }) => *affected,
                _ => 0,
            };
            Ok(Some(Value::Int(n)))
        }
        "columncount" => {
            let ob = obj.borrow();
            let n = match &ob.internal {
                Some(ObjectInternal::SqliteStmt { rows, .. }) => {
                    rows.first().map(|r| r.len() as i64).unwrap_or(0)
                }
                _ => 0,
            };
            Ok(Some(Value::Int(n)))
        }
        "bindvalue" | "bindparam" => {
            let mut ob = obj.borrow_mut();
            if let Some(ObjectInternal::SqliteStmt { bound, named, .. }) = &mut ob.internal {
                match a(args, 0) {
                    Value::Int(i) => {
                        let idx = (i - 1).max(0) as usize;
                        while bound.len() <= idx {
                            bound.push(Value::Null);
                        }
                        bound[idx] = a(args, 1);
                    }
                    Value::Str(s) => {
                        named.insert(
                            format!(":{}", crate::value::lossy(&s).trim_start_matches(':')),
                            a(args, 1),
                        );
                    }
                    _ => {}
                }
            }
            Ok(Some(Value::Bool(true)))
        }
        "closecursor" => Ok(Some(Value::Bool(true))),
        _ => Ok(None),
    }
}
