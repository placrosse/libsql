pub mod errors;

use errors::Error;

type Result<T> = std::result::Result<T, Error>;

pub struct Database {
    pub url: String,
}

impl Database {
    pub fn open(url: String) -> Database {
        Database { url }
    }

    pub fn close(&self) {
    }
}

pub struct Connection {
    raw: *mut sqlite3_sys::sqlite3,
}

unsafe impl Send for Connection {} // TODO: is this safe?

impl Connection {
    pub fn connect(url: String) -> Result<Connection> {
        let mut raw = std::ptr::null_mut();
        let err = unsafe {
            // FIXME: switch to libsql_sys
            sqlite3_sys::sqlite3_open_v2(
                url.as_ptr() as *const i8,
                &mut raw,
                sqlite3_sys::SQLITE_OPEN_READWRITE | sqlite3_sys::SQLITE_OPEN_CREATE,
                std::ptr::null(),
            )
        };
        match err {
            sqlite3_sys::SQLITE_OK => {}
            _ => {
                return Err(Error::ConnectionFailed(url.clone()));
            }
        }
        Ok(Connection { raw })
    }

    pub fn disconnect(&self) {
        unsafe {
            sqlite3_sys::sqlite3_close_v2(self.raw);
        }
    }

    pub fn execute(&self, sql: String) -> ResultSet {
        // TODO: submit execution to a work queue
        ResultSet { }
    }
}

pub struct ResultSet {
}

impl ResultSet {
    pub fn wait(&self) {
        // TODO: wait for execution to complete
    }

    pub fn row_count(&self) -> i32 {
        0
    }

    pub fn column_count(&self) -> i32 {
        0
    }
}