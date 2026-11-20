import {readFileSync} from 'node:fs';
import {JSDOM} from 'jsdom';
import {expect,it} from 'vitest';
it('shows changed and removed files as text, keeps stale reviews, and confirms removal', async () => {
 const page=readFileSync(new URL('../../../../../src/skills_page.html',import.meta.url),'utf8');
 const file=(data:string)=>({data:btoa(data),executable:false});
 const snapshot={skills:{example:{enabled:true,published:{id:1,files:{'SKILL.md':file('old'),'gone.md':file('old')}},draft:{id:2,base_revision:1,note:'<img src=x>',files:{'SKILL.md':file('new')}}}}};
 let status=409; let attempts=0;
 const dom=new JSDOM(page,{url:'https://proxy.test/skills',runScripts:'dangerously',beforeParse(w){w.confirm=()=>false;w.TextDecoder=TextDecoder;w.fetch=(async (_input:unknown,init?:RequestInit)=>{if(init?.method==='POST'){attempts++;return Response.json({error:'write failed'},{status});}return Response.json(snapshot);}) as typeof w.fetch;}});
 const {document}=dom.window;
 const click=(selector:string)=>(document.querySelector(selector) as HTMLButtonElement).click();
 const settle=()=>new Promise(r=>setTimeout(r,0));
 await settle();
 expect(document.querySelector('input[type=password]')).toBeNull();
 click('[data-action=review]'); await settle();
 expect(document.body.textContent).toContain('Removed gone.md');
 expect(document.body.textContent).toContain('<img src=x>'); expect(document.querySelector('img')).toBeNull();
 click('[data-action=publish]'); await settle(); expect(document.querySelector('#status')?.textContent).toContain('409');
 status=500; click('[data-action=publish]'); await settle(); expect(document.querySelector('#status')?.textContent).toContain('500');
 click('[data-action=remove]'); expect(attempts).toBe(2);
 dom.window.close();
});

it('shows the issued draft token and clears it on revoke', async () => {
 const page=readFileSync(new URL('../../../../../src/skills_page.html',import.meta.url),'utf8');
 const calls:string[]=[];
 const dom=new JSDOM(page,{url:'https://proxy.test/skills',runScripts:'dangerously',beforeParse(w){w.fetch=(async (url:string,init?:RequestInit)=>{
  calls.push(init?.method+' '+url);
  if(init?.method==='POST') return Response.json({token:'test-secret',expires_in_seconds:900});
  if(init?.method==='DELETE') return Response.json({});
  return Response.json({skills:{}});
 }) as typeof w.fetch;}});
 const {document}=dom.window;
 const click=(id:string)=>(document.getElementById(id) as HTMLButtonElement).click();
 const settle=()=>new Promise(r=>setTimeout(r,0));
 await settle();
 click('generate-draft-token'); await settle();
 expect(document.body.textContent).toContain('test-secret');
 click('revoke-draft-token'); await settle();
 expect(document.body.textContent).not.toContain('test-secret');
 expect(calls).toContain('DELETE /api/skills/draft-token');
 dom.window.close();
});

it('includes the agent note when importing a GitHub folder', async () => {
 const page=readFileSync(new URL('../../../../../src/skills_page.html',import.meta.url),'utf8');
 const calls:{url:string,body:unknown}[]=[];
 const dom=new JSDOM(page,{url:'https://proxy.test/skills',runScripts:'dangerously',beforeParse(w){w.fetch=(async (url:string,init?:RequestInit)=>{
  if(init?.method==='POST') calls.push({url,body:JSON.parse(init.body as string)});
  return Response.json({skills:{}});
 }) as typeof w.fetch;}});
 try {
  const document=dom.window.document;
  await new Promise(r=>setTimeout(r,0));
  const note=document.querySelector('#import textarea') as HTMLTextAreaElement;
  expect(note).not.toBeNull();
  note.value='Ask before installing rust-anything';
  (document.getElementById('folder') as HTMLInputElement).value='https://github.com/test/repo/tree/main/skill';
  document.getElementById('import')!.dispatchEvent(new dom.window.Event('submit',{cancelable:true}));
  await new Promise(r=>setTimeout(r,0));
  expect(calls).toEqual([{url:'/api/skills/import',body:{url:'https://github.com/test/repo/tree/main/skill',note:'Ask before installing rust-anything'}}]);
 } finally {dom.window.close();}
});

it('starts with a compact skill list, labelled icon actions and collapsed forms', async () => {
 const page=readFileSync(new URL('../../../../../src/skills_page.html',import.meta.url),'utf8');
 const snapshot={id:1,note:'Keep this note',metadata:{description:'Migrate applications to Rust'},files:{'SKILL.md':{data:btoa('skill body'),executable:false},'whole-app.md':{data:btoa('companion'),executable:false}}};
 const dom=new JSDOM(page,{url:'https://proxy.test/skills',runScripts:'dangerously',beforeParse(w){w.TextDecoder=TextDecoder;w.fetch=(async()=>Response.json({skills:{'rust-anything':{enabled:true,published:snapshot}}})) as typeof w.fetch;}});
 try {
  const {document}=dom.window;
  await new Promise(r=>setTimeout(r,0));
  const row=document.querySelector('#skills article')!;
  expect(row.querySelector('.skill-heading')?.textContent).toContain('rust-anything');
  expect(row.querySelector('.skill-description')?.textContent).toBe('Migrate applications to Rust');
  for(const button of row.querySelectorAll('button')) {
   expect(button.title).not.toBe('');
   expect(button.getAttribute('aria-label')).toBe(button.title);
   expect(button.querySelector('svg')).not.toBeNull();
  }
  const editor=document.querySelector('#editor-panel') as HTMLDetailsElement;
  const importer=document.querySelector('#import-panel') as HTMLDetailsElement;
  expect(editor.open).toBe(false); expect(importer.open).toBe(false);
  expect(editor.querySelector('summary')?.textContent).toBe('New skill');
  expect(importer.querySelector('summary')?.textContent).toBe('Import GitHub');
  expect(document.querySelector('#ref')).toBeNull();
  expect(document.querySelector('label[for="folder"]')?.textContent).toBe('URL');
  (row.querySelector('[data-action=edit]') as HTMLButtonElement).click();
  expect(editor.open).toBe(true);
  expect((document.querySelector('#note') as HTMLTextAreaElement).value).toBe('Keep this note');
  expect((document.querySelector('#content') as HTMLTextAreaElement).value).toBe('skill body');
 } finally {dom.window.close();}
});

it('keeps draft review collapsed until requested and requires review before publishing', async () => {
 const page=readFileSync(new URL('../../../../../src/skills_page.html',import.meta.url),'utf8');
 let writes=0;
 const dom=new JSDOM(page,{url:'https://proxy.test/skills',runScripts:'dangerously',beforeParse(w){w.TextDecoder=TextDecoder;w.fetch=(async(_url:unknown,init?:RequestInit)=>{if(init?.method==='POST') writes++;return Response.json({skills:{example:{enabled:false,draft:{id:2,base_revision:null,metadata:{description:'Example'},files:{'SKILL.md':{data:btoa('body'),executable:false}}}}}});}) as typeof w.fetch;}});
 try {
  const {document}=dom.window;
  await new Promise(r=>setTimeout(r,0));
  const review=document.querySelector('.skill-review') as HTMLDetailsElement;
  expect(review.open).toBe(false);
  expect(document.querySelector('[data-action=publish]')?.closest('.skill-review')).toBe(review);
  (document.querySelector('[data-action=review]') as HTMLButtonElement).click();
  expect(review.open).toBe(true);
  expect(writes).toBe(0);
 } finally {dom.window.close();}
});


it('builds each draft comparison only on first open', async () => {
 const page=readFileSync(new URL('../../../../../src/skills_page.html',import.meta.url),'utf8');
 const snapshot={skills:{example:{enabled:true,published:{id:1,files:{'SKILL.md':{data:btoa('PUBLISHED-CANARY'),executable:false}}},draft:{id:2,base_revision:1,files:{'SKILL.md':{data:btoa('DRAFT-CANARY'),executable:false}}}}}};
 const dom=new JSDOM(page,{url:'https://proxy.test/skills',runScripts:'dangerously',beforeParse(w){w.TextDecoder=TextDecoder;w.fetch=(async()=>Response.json(snapshot)) as typeof w.fetch;}});
 try {
  const settle=()=>new Promise(r=>setTimeout(r,0));
  await settle();
  const {document}=dom.window;
  const panel=document.querySelector('.skill-review') as HTMLDetailsElement;
  expect(panel.open).toBe(false);
  expect(panel.querySelector('pre')).toBeNull();
  expect(panel.textContent).not.toContain('DRAFT-CANARY');
  (document.querySelector('[data-action=review]') as HTMLButtonElement).click();
  await settle();
  expect(panel.textContent).toContain('PUBLISHED-CANARY');
  expect(panel.textContent).toContain('DRAFT-CANARY');
  const count=panel.querySelectorAll('pre').length;
  panel.open=false; await settle();
  panel.open=true; await settle();
  expect(panel.querySelectorAll('pre')).toHaveLength(count);
 } finally {dom.window.close();}
});
