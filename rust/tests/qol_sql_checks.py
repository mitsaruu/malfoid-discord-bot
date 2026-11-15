"""Проверка опасных SQL-операций QoL без Discord и запуска Rust (только stdlib Python).

Запуск из rust/: python tests/qol_sql_checks.py
Берёт SQL непосредственно из Rust-исходников; это не заменяет cargo test.
"""
import json
import re
import sqlite3
import unittest
from pathlib import Path

SRC = Path(__file__).resolve().parents[1] / "src"


def sql(file, prefix):
    literals = re.findall(r'"([^"\r\n]+)"', (SRC / file).read_text(encoding="utf-8"))
    return next(value for value in literals if value.startswith(prefix))


class QueueChecks(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(":memory:")
        self.db.execute("CREATE TABLE queue(id INTEGER PRIMARY KEY,guild_id INTEGER,url TEXT,title TEXT)")
        for column, kind in [("details", "TEXT"), ("sort_order", "INTEGER")]:
            self.db.execute(f"ALTER TABLE queue ADD COLUMN {column} {kind}")

    def tearDown(self):
        self.db.close()

    def add(self, user, nonce, url="same", front=False):
        prefix = "SELECT COALESCE(MIN" if front else "SELECT COALESCE(MAX"
        order = self.db.execute(sql("music_qol.rs", prefix), (1,)).fetchone()[0]
        meta = json.dumps({"requester": user, "request_id": nonce})
        self.db.execute(sql("music_qol.rs", "INSERT INTO queue(guild_id,url,title,sort_order,details) VALUES"),
                        (1, url, "song", order, meta))

    def rows(self):
        return self.db.execute(sql("music_qol.rs", "SELECT id,url,title,details FROM queue WHERE").replace("?1", "1")).fetchall()

    def remove(self, row):
        return self.db.execute(sql("music_qol.rs", "DELETE FROM queue WHERE guild_id=?1 AND id=?2"),
                               (1, row[0], row[1], row[2], row[3] or "null")).rowcount

    def test_duplicates_keep_their_requester(self):
        self.add(1, 100)
        self.add(2, 200)
        a, b = self.rows()
        self.assertEqual(self.remove(a), 1)
        self.assertEqual(json.loads(self.rows()[0][3])["requester"], 2)
        self.assertEqual(self.rows()[0][0], b[0])

    def test_confirmed_clear_does_not_remove_new_duplicate_with_reused_id(self):
        self.add(1, 100)
        old = self.rows()[0]
        self.remove(old)
        self.add(1, 200)
        self.assertEqual(old[0], self.rows()[0][0])  # SQLite действительно переиспользовал id
        self.assertEqual(self.remove(old), 0)
        self.assertEqual(len(self.rows()), 1)

    def test_return_to_front_and_move_preserve_metadata(self):
        self.add(1, 100)
        self.add(2, 200, "other")
        self.add(3, 300, "returned", front=True)
        rows = self.rows()
        self.assertEqual(rows[0][1], "returned")
        move = sql("music_qol.rs", "UPDATE queue SET sort_order=?1 WHERE guild_id")
        for i, row in enumerate(reversed(rows), 1):
            self.db.execute(move, (i, 1, row[0]))
        self.assertEqual([json.loads(r[3])["requester"] for r in self.rows()], [2, 1, 3])

    def test_second_sale_does_not_pay_again(self):
        self.db.execute("CREATE TABLE pets(id INTEGER,guild_id INTEGER,user_id INTEGER,level INTEGER,spent INTEGER,last_collect REAL)")
        self.db.execute("CREATE TABLE economy(guild_id INTEGER,user_id INTEGER,balance INTEGER)")
        self.db.execute("INSERT INTO pets VALUES(1,1,2,3,2000,10.0)")
        self.db.execute("INSERT INTO economy VALUES(1,2,500)")
        for _ in range(2):
            deleted = self.db.execute(sql("pets.rs", "DELETE FROM pets WHERE id=?1 AND guild_id"), (1,1,2,3,2000,10.)).rowcount
            if deleted:
                self.db.execute(sql("pets.rs", "UPDATE economy SET balance=balance+?1"), (1000,1,2))
        self.assertEqual(self.db.execute("SELECT balance FROM economy").fetchone()[0], 1500)


if __name__ == "__main__":
    unittest.main()
