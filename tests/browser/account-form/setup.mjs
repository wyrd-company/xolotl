import {writeFileSync} from 'node:fs';
const directory=process.env.ACCOUNT_FORM_FIXTURE_DIR;
if(!directory)throw Error('Set ACCOUNT_FORM_FIXTURE_DIR to your scratch directory');
const vault='http://127.0.0.1:35200/v1';
async function api(path,body){const r=await fetch(vault+path,{method:body===undefined?'GET':'POST',headers:{'X-Vault-Token':'fixture-root','content-type':'application/json'},body:body===undefined?undefined:JSON.stringify(body)});const t=await r.text();if(!r.ok)throw Error(`${path}: ${r.status} ${t}`);return t?JSON.parse(t):{};}
await api('/sys/auth/approle',{type:'approle'});
await api('/sys/policies/acl/fixture',{policy:'path "secret/*" { capabilities = ["create", "read", "update", "delete", "list"] }'});
await api('/auth/approle/role/fixture',{token_policies:['fixture']});
const role=(await api('/auth/approle/role/fixture/role-id')).data.role_id;
const secret=(await api('/auth/approle/role/fixture/secret-id',{})).data.secret_id;
await api('/secret/data/xolotl/oidc',{data:{client_secret:'synthetic-oidc-secret'}});
await api('/secret/data/xolotl/admin',{data:{token:'synthetic-admin-token'}});
writeFileSync(`${directory}/config.toml`,`[server]\nlisten = "0.0.0.0:35317"\n[database]\npath = "/data/xolotl.db"\n[vault]\naddress = "http://127.0.0.1:35445"\nmount = "secret"\nprefix = "xolotl"\n[vault.approle]\nrole_id = "${role}"\nsecret_id = "${secret}"\n[ui.oidc]\nissuer = "http://localhost:35444"\nclient_id = "xolotl"\nredirect_url = "http://localhost:35317/admin/session/callback"\nadmin_group = "admins"\n[catalog]\nmetadata_url = ""\n[catalog.static_models]\nfixture = ["fixture-model"]\n[telemetry]\nlog_filter = "warn"\n`);
console.log('Synthetic AppRole, OIDC and admin bootstrap seeded; config written.');
writeFileSync(`${directory}/config.toml`, `\n[subscription]\nusage_poll_secs = 0\n[subscription.endpoints]\ncodex_backend = "http://localhost:35444"\ncodex_auth = "http://localhost:35444"\n`, {flag:'a'});
