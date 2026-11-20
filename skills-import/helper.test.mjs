import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, readFile, readdir, rm, symlink, lstat } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { resolveSource } from 'skills';
import { choices, materialize, validateRequest, run } from './helper.mjs';

const commit = 'a'.repeat(40);
test('reject unsafe sources and selections before fetching', () => {
  for (const url of ['file:///etc', '/tmp/repo', 'https://user:secret@github.com/a/b', 'http://github.com/a/b', 'https://evil.test/a/b', 'https://github.com/a/b/tree/main/skill']) {
    assert.throws(() => validateRequest({action:'discover', url}));
  }
  assert.equal(validateRequest({action:'discover',url:'https://github.com/a/b.git'}).url, 'https://github.com/a/b');
  for (const path of ['../secret', '/absolute', 'folder/../secret', 'folder\\secret']) {
    assert.throws(() => validateRequest({action:'import',url:'https://github.com/a/b',commit,path,name:'example'}));
  }
  assert.throws(() => validateRequest({action:'import',url:'https://github.com/a/b',commit:'main',path:'skill',name:'example'}));
});
test('duplicate names expand into explicit folder choices', () => {
  assert.deepEqual(choices([{name:'example',description:'Example',path:'skills/example',duplicates:['plugin/skills/example']}]), [
    {name:'example',description:'Example',path:'skills/example'},
    {name:'example',description:'Alternate folder with this skill name',path:'plugin/skills/example'},
  ]);
  assert.deepEqual(choices([{name:'Unsupported_Name',description:'Invalid registry name',path:'skill'}]),[]);
});
test('copy companion files without executing scripts; reject links; clean staging', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'aiproxy-helper-test-'));
  try {
    const fixture = join(dir,'fixture'), staging = join(dir,'staging');
    await mkdir(join(fixture,'scripts'),{recursive:true}); await mkdir(staging);
    await writeFile(join(fixture,'SKILL.md'),'---\nname: example\ndescription: Example\n---\nInstructions');
    await writeFile(join(fixture,'whole-app.md'),'Companion');
    await writeFile(join(fixture,'__proto__'),'Retain this file');
    await writeFile(join(fixture,'scripts','report.sh'),'#!/bin/sh\ntouch "'+join(dir,'SCRIPT-RAN')+'"\n',{mode:0o755});
    let source = await resolveSource(fixture);
    try {
      const result=await materialize(source,'example',staging);
      assert.equal(Buffer.from(result.files['whole-app.md'].data,'base64').toString(),'Companion');
      assert.equal(result.files['scripts/report.sh'].executable,true);
      assert(Object.hasOwn(result.files,'__proto__'));
      await assert.rejects(lstat(join(dir,'SCRIPT-RAN')),{code:'ENOENT'});
      assert.deepEqual(await readdir(staging),[]);
    } finally {await source.dispose();}
    await symlink('/etc/passwd',join(fixture,'unsafe-link'));
    source=await resolveSource(fixture);
    try {
      await assert.rejects(materialize(source,'example',staging), /symlink/i);
      assert.deepEqual(await readdir(staging),[]);
    } finally {await source.dispose();}
    await rm(join(fixture,'unsafe-link'));
    await writeFile(join(fixture,'oversize.md'),Buffer.alloc(512*1024+1));
    source=await resolveSource(fixture);
    try {await assert.rejects(materialize(source,'example',staging), /large|limit/i);}
    finally {await source.dispose();}
    assert.deepEqual(await readdir(staging),[]);
  } finally {await rm(dir,{recursive:true,force:true});}
});

test('live repository discovery copies the selected full-path skill at the discovered commit', {skip:process.env.AIPROXY_LIVE_SKILLS_TEST !== '1'}, async () => {
  const url='https://github.com/erkamyaman/rust-anything';
  const found=await run({action:'discover',url});
  const selected=found.skills.find(skill=>skill.path==='skills/rust-anything');
  assert(selected);
  const imported=await run({action:'import',url,commit:found.commit,path:selected.path,name:selected.name});
  assert.equal(imported.commit,found.commit);
  assert(imported.files['SKILL.md']);
  assert(imported.files['whole-app.md']);
});

test('file count limit includes files in nested directories', async () => {
  const dir=await mkdtemp(join(tmpdir(),'aiproxy-helper-count-'));
  let source;
  try {
    const fixture=join(dir,'fixture');
    await mkdir(join(fixture,'assets'),{recursive:true});
    await writeFile(join(fixture,'SKILL.md'),'---\nname: example\ndescription: Example\n---\nInstructions');
    for(let i=0;i<127;i++) await writeFile(join(fixture,'assets','file-'+i),'x');
    source=await resolveSource(fixture);
    const copied=await materialize(source,'example',dir);
    assert.equal(Object.keys(copied.files).length,128);
    await writeFile(join(fixture,'assets','one-too-many'),'x');
    await assert.rejects(materialize(source,'example',dir),/count|limit/i);
    assert(!(await readdir(dir)).some(name=>name.startsWith('copy-')));
  } finally {
    await source?.dispose();
    await rm(dir,{recursive:true,force:true});
  }
});
