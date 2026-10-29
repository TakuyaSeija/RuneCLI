#!/usr/bin/env python3
import json
from pathlib import Path
import sqlite3
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
import build_session_data as builder
from render_source import SourceRenderer


class RecordingTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.renderer = SourceRenderer()

    @classmethod
    def tearDownClass(cls):
        cls.renderer.close()

    def test_no_irreversible_preview_truncation(self):
        text = "\n".join("x" * 300 + str(i) for i in range(19)) + "\n"
        rows = builder.lines(text)
        self.assertEqual(len(rows), 19)
        self.assertEqual(rows[-1], "x" * 300 + "18")

    def test_redaction_preserves_row_count(self):
        text = 'safe\nhttp://localhost:1234/music\n/home/person/project/file.js\n192.168.1.3\nsecrets.vault\nlast'
        rows = builder.lines(text)
        self.assertEqual(len(rows), 6)
        raw = "\n".join(rows)
        self.assertFalse(builder.ADDRESS.search(raw))
        self.assertFalse(any(word in raw.lower() for word in builder.DENY))
        self.assertEqual(rows[0], 'safe')
        self.assertEqual(rows[-1], 'last')
        self.assertEqual(builder.clean('API :3001, Vite :5173'), 'API :…, Vite :…')

    def test_only_genuine_context_notifications_are_exported(self):
        events = [
            {'role': 'context', 'created_at': 1, 'content': '<system_notice>ordinary context</system_notice>'},
            {'role': 'user', 'created_at': 2, 'content': '<system_notification>quoted by user</system_notification>'},
            {'role': 'context', 'created_at': 3, 'content': '<system_notification>Background task completed.</system_notification>'},
        ]
        self.assertEqual(builder.notifications(events), [{'at': 3, 'text': 'Background task completed.'}])

    def test_context_is_exact_saved_model_not_a_similar_name(self):
        db = sqlite3.connect(':memory:')
        db.execute('CREATE TABLE provider_models(id TEXT,info TEXT)')
        db.executemany('INSERT INTO provider_models VALUES(?,?)', [('route/model', '{"context":1000000}'), ('other/model', '{"context":1048576}')])
        self.assertEqual(builder.model_context(db, 'route/model'), 1000000)
        with self.assertRaises(ValueError):
            builder.model_context(db, 'model')

    def test_cli_diff_keeps_equal_context_and_real_add_remove_rows(self):
        before = 'one\ntwo\nthree\nfour\nfive\n'
        after = 'one\ntwo\nnew\nfour\nfive\n'
        rows = self.renderer.diff(before, after)
        self.assertEqual(rows, [[' ', 'one'], [' ', 'two'], ['-', 'three'], ['+', 'new'], [' ', 'four'], [' ', 'five']])
        self.assertEqual(self.renderer.diff(before, before), [])

    def test_terminal_pulse_palette_comes_from_each_cli_theme(self):
        css = (Path(__file__).resolve().parents[1] / 'UI/Static/Styles/Terminal.css').read_text()
        for theme in ['amber', 'violet', 'cyan']:
            palette = {role: color for _, role, color in self.renderer.request('palette', theme)}
            for role in ['Text', 'Faint', 'Accent', 'Rule', 'Think', 'Good', 'Bad']:
                self.assertIn(palette[role], css, (theme, role))

    def test_cli_lexer_keeps_every_character_and_semantic_ink(self):
        source = 'const title = "Music 世界"; play(42); // comment'
        tokens = self.renderer.tokens('js', source)
        self.assertEqual(''.join(t['Text'] for t in tokens), source)
        classes = {t['Class'] for t in tokens}
        self.assertTrue({'syntax-keyword', 'syntax-string', 'syntax-number', 'syntax-function', 'syntax-comment'} <= classes)

    def test_checked_in_source_is_complete_and_public(self):
        raw = (Path(__file__).resolve().parents[1] / 'UI/Data/Session655.json').read_text()
        rec = json.loads(raw)
        public_text = '\n'.join(builder.text_values(rec))
        self.assertFalse(builder.ADDRESS.search(public_text))
        self.assertFalse(any(word in public_text.lower() for word in builder.DENY))
        self.assertGreater(rec['start_at'], 0)
        self.assertAlmostEqual((rec['turns'][-1]['at'] - rec['start_at']) / 1000, 2136.825)
        self.assertEqual(rec['recorded_mode'], 'full')
        self.assertEqual(rec['mode'], 'ask')
        self.assertEqual(rec['context'], 1000000)
        self.assertEqual(rec['final']['turns'], 1)
        self.assertEqual(rec['final']['requests'], 145)
        self.assertEqual(rec['stream_timing'], 'request_boundaries_only')
        self.assertEqual(rec['notifications'], [])
        self.assertEqual(sum(len(t['calls']) for t in rec['turns']), 207)
        for turn in rec['turns']:
            for call in turn['calls']:
                self.assertNotIn('cut', call)
                self.assertGreaterEqual(call['started_at'], turn['at'])
                self.assertGreaterEqual(call['ended_at'], call['started_at'])
                self.assertEqual(call['ms'], call['ended_at'] - call['started_at'])
                if call['n'] == 'terminal_use':
                    self.assertEqual(''.join(t['Text'] for t in call['command_tokens']), call.get('cmd', ''))
                if call['n'] == 'create_file':
                    self.assertEqual(len(call['body']), call['total'])
                    self.assertEqual(len(call['syntax']), len(call['body']))
        self.assertEqual(rec['palette']['CodeString'], '#de8950')


if __name__ == '__main__':
    unittest.main()
