// Prove the precision/range guards fail when broken; restore source after each run.
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
const root=fileURLToPath(new URL('../../../',import.meta.url));
const output=process.argv[2];
if(!output||!path.isAbsolute(output)||output.startsWith(root))throw Error('Pass an absolute scratch output directory outside the worktree');
mkdirSync(output,{recursive:true});
const report={status:'failed',mutations:[]};
const reportPath=path.join(output,'mutation-result.json');
writeFileSync(reportPath,JSON.stringify(report,null,2));
const cases=[
 {name:'browser-upper-bound',file:'ui/src/lib/priority.ts',from:'<= 9223372036854775807n',to:'<= 9223372036854775808n',command:'pnpm',args:['--dir','ui','test','src/lib/priority.test.ts'],marker:'rejects invalid priority 9223372036854775808'},
 {name:'server-decimal-grammar',file:'src/server/admin.rs',from:"if digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit())",to:'if false',command:'cargo',args:['test','--locked','account_priority_decimal_roundtrips_and_validates_signed_range','--lib'],marker:'test server::tests::account_priority_decimal_roundtrips_and_validates_signed_range ... FAILED'},
 {name:'minimum-priority-selection',file:'src/subscription/mod.rs',from:'std::cmp::Reverse(r.priority)',to:'-r.priority',command:'cargo',args:['test','--locked','priority_selection_handles_signed_extremes','--lib'],marker:'test subscription::tests::priority_selection_handles_signed_extremes ... FAILED'},
];
for(const mutation of cases){
 const file=path.join(root,mutation.file),source=readFileSync(file,'utf8');
 if(!source.includes(mutation.from))throw Error(`Mutation target missing: ${mutation.name}`);
 let run;
 try{
  writeFileSync(file,source.replace(mutation.from,mutation.to));
  run=spawnSync(mutation.command,mutation.args,{cwd:root,encoding:'utf8',maxBuffer:10*1024*1024});
 }finally{writeFileSync(file,source)}
 const log=(run.stdout||'')+(run.stderr||'');
 writeFileSync(path.join(output,`${mutation.name}.log`),log);
 const killed=run.status!==0&&log.includes(mutation.marker);
 report.mutations.push({name:mutation.name,killed,exit:run.status});
 writeFileSync(reportPath,JSON.stringify(report,null,2));
 if(!killed)throw Error(`Mutation was not caught by its behavioral assertion: ${mutation.name}`);
}
report.status='passed';
writeFileSync(reportPath,JSON.stringify(report,null,2));
console.log(JSON.stringify(report,null,2));
