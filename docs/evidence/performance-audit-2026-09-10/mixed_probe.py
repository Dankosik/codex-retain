import hashlib,json,os,pathlib,shlex,sqlite3,subprocess,sys,time,uuid
sys.dont_write_bytecode=True
REPO=pathlib.Path(__file__).resolve().parents[3]
sys.path.insert(0,str(REPO/'scripts'))
import benchmark,fixture
ROOT=benchmark.benchmark_root(sys.argv[1])
case=json.loads((ROOT/'cases/retaincleanup-10000.json').read_text())
case.update(operation='preview',count=1000,fixture_root=str(ROOT/'fixtures/retainpreview-1000'))
casepath=ROOT/'cases/retainpreview-1000.json'
fixture.write_json(casepath,case)
benchmark.prepare(casepath)
root=pathlib.Path(case['fixture_root']); home=root/'codex'; db=home/'state_5.sqlite'
fixture.owned_root(root)
created=[]
with sqlite3.connect(db) as c:
    for n in range(99000):
        ident=str(uuid.uuid5(fixture.NAMESPACE,'active-'+str(n)))
        path=home/'sessions'/('rollout-2026-01-01T00-00-00-'+ident+'.jsonl')
        data=(json.dumps({'type':'session_meta','payload':{'id':ident,'history_mode':'legacy'}})+'\n').encode()
        path.write_bytes(data)
        created.append((ident,str(path)))
    c.executemany("INSERT INTO threads(id,rollout_path,created_at,updated_at,source,model_provider,cwd,title,sandbox_policy,approval_mode,archived,history_mode) VALUES(?,?,0,0,'cli','openai','/fixture','Synthetic active','read-only','never',0,'legacy')",created)
    c.execute('UPDATE codex_retain_epochs SET archived_since=?',(case['now'],))
    c.commit(); c.execute('PRAGMA wal_checkpoint(TRUNCATE)')
print('Prepared 1000 archives + 99000 active synthetic threads and files',flush=True)
env=benchmark.environment(case)
command=benchmark.timed_command(case)
results=[]
def signature():
    h=hashlib.sha256()
    for directory in ('sessions','archived_sessions'):
        for p in sorted((home/directory).iterdir()):
            h.update(str(p.relative_to(home)).encode()); h.update(hashlib.sha256(p.read_bytes()).digest())
    return h.hexdigest()
original_files=signature()
def check(expected_archives,eligible):
    output=json.loads(benchmark.execute(benchmark.command(case),env=env))
    assert output['examined']==expected_archives and output['eligible']==eligible and output['deleted']==0,output
    with sqlite3.connect(db) as c:
        assert c.execute('PRAGMA integrity_check').fetchone()[0]=='ok'
        assert c.execute('SELECT COUNT(*) FROM threads').fetchone()[0]==100000
    assert len(list((home/'sessions').iterdir()))==99000
    assert len(list((home/'archived_sessions').iterdir()))==1000
    return {'examined':output['examined'],'eligible':output['eligible'],'deleted':output['deleted'],'skipped':output['skipped']}
def measure(name,archives,eligible):
    pre=check(archives,eligible)
    subprocess.run(['/opt/homebrew/bin/hyperfine','--shell=none','--warmup','3','--runs','5','--output=pipe','--command-name',name,'--export-json',str(ROOT/(name+'-hyperfine.json')),command],check=True)
    # Separate instrumented parent-only RSS observation; polling is excluded from hyperfine samples.
    rss=[]
    with open(os.devnull,'wb') as sink:
        p=subprocess.Popen(benchmark.command(case),env=env,stdin=subprocess.DEVNULL,stdout=sink,stderr=subprocess.PIPE)
        while p.poll() is None:
            observation=subprocess.run(['/bin/ps','-o','rss=','-p',str(p.pid)],capture_output=True,text=True)
            if observation.stdout.strip(): rss.append(int(observation.stdout.strip()))
            time.sleep(.01)
        assert p.returncode==0,p.stderr.read()
    post=check(archives,eligible)
    result={'name':name,'postconditions':post,'parent_rss_kib_sampled_max':max(rss,default=None),'parent_rss_samples':len(rss)}
    results.append(result); print(json.dumps(result),flush=True)
measure('mixed-none-due',1000,0)
with sqlite3.connect(db) as c:
    ident=c.execute('SELECT id FROM threads WHERE archived=1 ORDER BY id LIMIT 1').fetchone()[0]
    c.execute('UPDATE codex_retain_epochs SET archived_since=? WHERE thread_id=?',(case['now']-40*fixture.DAY,ident))
    c.commit();c.execute('PRAGMA wal_checkpoint(TRUNCATE)')
measure('mixed-one-due',1000,1)
with sqlite3.connect(db) as c:
    c.execute('UPDATE threads SET archived=1,archived_at=? WHERE archived=0',(case['now'],))
    c.execute('UPDATE codex_retain_epochs SET archived_since=?',(case['now'],))
    c.commit();c.execute('PRAGMA wal_checkpoint(TRUNCATE)')
measure('100k-archives-none-due',100000,0)
assert signature()==original_files
fixture.write_json(ROOT/'mixed-summary.json',{'observations':results,'files_sha256_before_after':original_files,'files_unchanged':True,'source':'existing exact candidate release executable; no code change','sample_plan':'warmup 3, runs 5, warm caches, sequential cases, shared desktop; parent ps observations separate from hyperfine','limits':['100k unexpired metadata rows intentionally leave 99k files in sessions; no due-file validation in that case','parent RSS sampled maximum is a lower bound, not heap allocation or total process tree memory','not an optimized candidate comparison']})
