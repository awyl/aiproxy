import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { mkdtemp, readdir, lstat, readFile, rm } from 'node:fs/promises';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { resolveSource, installSkills } from 'skills';

const exec = promisify(execFile);
const sha = value => typeof value === 'string' && /^[a-f0-9]{40}$/i.test(value);
const slug = value => typeof value === 'string' && value !== '.' && value !== '..' && /^[A-Za-z0-9_.-]{1,128}$/.test(value);
const skillName = value => typeof value === 'string' && value.length <= 64 && /^[a-z0-9]+(?:-[a-z0-9]+)*$/.test(value);
const folder = value => value === '.' || (typeof value === 'string' && value.length <= 512 && value.split('/').every(slug));

export function validateRequest(input) {
  const match = typeof input.url === 'string' && input.url.match(/^https:\/\/github\.com\/([A-Za-z0-9_.-]+)\/([A-Za-z0-9_.-]+?)\/?$/);
  if (!match) throw new Error('Enter a public HTTPS GitHub repository URL, not a skill-folder URL.');
  const owner = match[1], repo = match[2].replace(/\.git$/, '');
  if (!slug(owner) || !slug(repo)) throw new Error('Invalid repository name.');
  const result = {...input, url: 'https://github.com/'+owner+'/'+repo};
  if (input.action === 'import') {
    if (!sha(input.commit) || !folder(input.path) || !skillName(input.name)) throw new Error('Invalid skill selection; use Find skills again.');
  } else if (input.action === 'update') {
    if (!folder(input.path) || !skillName(input.name) || (input.ref != null && (typeof input.ref !== 'string' || input.ref.length > 256 || !input.ref.split('/').every(slug)))) throw new Error('Invalid saved skill source.');
  } else if (input.action !== 'discover') throw new Error('Unknown importer action.');
  return result;
}

export function choices(skills) {
  return skills.flatMap(skill => [
    {name:skill.name, description:skill.description, path:skill.path},
    ...(skill.duplicates ?? []).map(path => ({name:skill.name,description:'Alternate folder with this skill name',path})),
  ]).filter(skill => folder(skill.path) && skillName(skill.name));
}

async function collect(root, prefix = '', state = {files:Object.create(null), total:0}) {
  for (const entry of await readdir(join(root,prefix), {withFileTypes:true})) {
    const path = prefix ? prefix+'/'+entry.name : entry.name;
    if (entry.isDirectory()) {
      await collect(root,path,state);
      continue;
    }
    if (!entry.isFile()) throw new Error('Skills must contain only regular files; symlinks are not supported.');
    const stat = await lstat(join(root,path));
    if (!stat.isFile() || stat.size > 512*1024) throw new Error('Skill file is too large or not regular.');
    if (Object.keys(state.files).length >= 128 || state.total + stat.size > 2*1024*1024) throw new Error('Skill exceeds file count or size limit.');
    const bytes = await readFile(join(root,path));
    if (bytes.length !== stat.size) throw new Error('Skill file changed while reading.');
    state.total += bytes.length;
    state.files[path] = {data:bytes.toString('base64'), executable:!!(stat.mode & 0o111), blob:null};
  }
  return state.files;
}

export async function materialize(source, name, staging) {
  const dir = await mkdtemp(join(staging, 'copy-'));
  try {
    const result = await installSkills(source, {
      cwd:dir, skills:[name], agents:['universal'], mode:'copy',
      lockfile:false, telemetry:false, duplicates:'error', symlinks:'reject',
    });
    if (result.failed.length || result.skipped.length || result.installed.length !== 1) throw new Error(result.failed[0]?.message ?? 'Selected skill could not be copied.');
    const root = join(dir,'.agents','skills',name);
    if (result.installed[0].path !== root) throw new Error('Unexpected skill destination.');
    return {name,files:await collect(root)};
  } finally { await rm(dir,{recursive:true,force:true}); }
}

async function remoteCommit(url, ref) {
  // Resolve the moving ref once. Discovery and import then use the immutable SHA.
  const target = ref ? (sha(ref) ? null : ['refs/heads/'+ref,'refs/tags/'+ref,'refs/tags/'+ref+'^{}']) : ['HEAD'];
  if (target === null) return ref;
  const {stdout} = await exec('git', ['ls-remote',url,...target], {timeout:20000,maxBuffer:64*1024});
  const rows = stdout.trim().split('\n').map(line => line.split(/\s+/));
  const preferred = rows.find(row => row[1] === 'refs/heads/'+ref)
    ?? rows.find(row => row[1] === 'refs/tags/'+ref+'^{}')
    ?? rows.find(row => row[1] === 'refs/tags/'+ref)
    ?? rows.find(row => row[1] === 'HEAD');
  if (!preferred || !sha(preferred[0])) throw new Error('Repository or tracked ref was not found.');
  return preferred[0];
}

export async function run(input) {
  input = validateRequest(input);
  const commit = input.action === 'import' ? input.commit : await remoteCommit(input.url,input.ref);
  const url = input.action === 'discover' || input.path === '.' ? input.url : input.url+'/tree/'+commit+'/'+input.path;
  const source = await resolveSource(url, {ref:commit, fullDepth:input.action === 'discover'});
  try {
    if (source.source.commit !== commit) throw new Error('Repository commit did not match the requested snapshot.');
    if (input.action === 'discover') {
      const skills = choices(source.skills);
      if (!skills.length || skills.length > 256) throw new Error('Repository contains no supported skills, or too many skills.');
      return {commit,skills};
    }
    const actual = source.skills.find(skill => skill.name === input.name);
    if (!actual || (actual.path !== input.path && actual.path !== '.')) throw new Error('Selected skill was not found at the chosen folder.');
    return {...await materialize(source,input.name,process.cwd()), commit};
  } finally {await source.dispose();}
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    let body = '';
    for await (const chunk of process.stdin) {
      body += chunk;
      if (body.length > 16384) throw new Error('Importer request is too large.');
    }
    console.log(JSON.stringify(await run(JSON.parse(body))));
  } catch (error) {
    console.log(JSON.stringify({error:error.message ?? 'Skill import failed.'}));
    process.exitCode = 1;
  }
}
