"""Integration tests for the real aizen sandbox review CLI on a disposable profile."""
import concurrent.futures
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


class Reviews(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.workspace = self.root / 'project'
        self.workspace.mkdir()
        self.dest = self.root / 'exports'
        self.dest.mkdir()
        self.env = {**os.environ, 'AIZEN_HOME': str(self.root/'profile'), 'AIZEN_NO_UPDATE_CHECK':'1'}

    def call(self, action, *, topic='one', cwd=None, fails=False, **fields):
        command = [os.environ['AIZEN_TEST_BIN'], 'sandbox','review','--topic',topic]
        result = subprocess.run(command, input=json.dumps(dict(action=action, **fields)).encode(), cwd=cwd or self.workspace,
                                capture_output=True, env=self.env, timeout=30)
        self.assertEqual(result.returncode != 0, fails, (result.stdout, result.stderr))
        return json.loads(result.stdout)

    def test_review_export_and_scopes(self):
        self.assertIsNone(self.call('read')['review'])
        doc = '# Aizen\n\nA real Markdown document with Unicode ✓'
        sha = hashlib.sha256(doc.encode()).hexdigest()
        self.call('submit', expected=0, document=doc, sha256='incorrect', fails=True)
        self.call('submit', expected=0, document=doc, sha256=sha)
        self.assertIsNone(self.call('read', topic='two')['review'])
        other = self.root/'other'
        other.mkdir()
        self.assertIsNone(self.call('read', cwd=other)['review'])
        self.call('revise', version=1, note='Add the next step')
        self.call('deliver', version=1, directory=str(self.dest), fails=True)
        updated=doc+'\n\nNext step: review the result.\n'
        sha2=hashlib.sha256(updated.encode()).hexdigest()
        self.call('submit', expected=1, document=updated, sha256=sha2)
        self.assertEqual(self.call('read',version=1)['review']['document'],doc)
        self.call('deliver', version=1, directory=str(self.dest), fails=True)
        def approve(_):
            return self.call('deliver', version=2, directory=str(self.dest))['review']
        with concurrent.futures.ThreadPoolExecutor(max_workers=3) as pool:
            receipts = list(pool.map(approve, range(3)))
        self.assertEqual(len(set(r['destination'] for r in receipts)),1)
        file = next(self.dest.glob('*.md'))
        self.assertEqual(file.read_bytes(),updated.encode())
        self.assertEqual(len(list(self.dest.iterdir())),1)
        file.write_bytes(b'External edit')
        self.call('deliver',version=2,directory=str(self.dest),fails=True)
        self.assertEqual(file.read_bytes(),b'External edit')
        self.call('discard',version=1)
        self.assertEqual(self.call('read',version=1)['review']['document'],'')
        self.assertEqual(self.call('read')['review']['sha256'],sha2)

    def test_sample_retry_and_storage(self):
        for i in range(50):
            item=self.call('sample', expected=i)
            self.assertTrue(item['review']['sample'])
            self.call('discard',version=i+1)
        view=self.call('read')
        self.assertLessEqual(len(view['history']),33)
        journal = next((self.root/'profile').rglob('documents.json'))
        self.assertLess(journal.stat().st_size,12*1024)
        print('50 discarded review versions; journal bytes:', journal.stat().st_size)
        self.call('sample',expected=50)
        self.call('revise',version=51,note='Update acceptance')
        self.call('sample',expected=51)
        self.assertNotEqual(self.call('read',version=51)['review']['sha256'], self.call('read')['review']['sha256'])
        self.call('discard',version=52)
        self.call('discard',version=52)

    def test_restart_after_export_and_conflict(self):
        self.call('sample',expected=0)
        journal=next((self.root/'profile').rglob('documents.json'))
        record=json.loads(journal.read_bytes())
        dest=self.dest/'staged.md'
        record['versions'][0].update(status='delivering',destination=str(dest))
        journal.write_text(json.dumps(record),encoding='utf-8')
        dest.write_text('somebody else',encoding='utf-8')
        self.call('deliver',version=1,fails=True)
        self.assertEqual(dest.read_text(),'somebody else')
        dest.write_bytes(record['versions'][0]['document'].encode())
        self.assertEqual(self.call('deliver',version=1)['review']['status'],'delivered')

if __name__=='__main__':
    unittest.main()
