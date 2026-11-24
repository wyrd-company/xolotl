import {writeFileSync} from 'node:fs';
const vault='http://127.0.0.1:34200/v1';
async function api(path,body){const r=await fetch(vault+path,{method:body===undefined?'GET':'POST',headers:{'X-Vault-Token':'fixture-root','content-type':'application/json'},body:body===undefined?undefined:JSON.stringify(body)});const t=await r.text();if(!r.ok)throw Error(`${path}: ${r.status} ${t}`);return t?JSON.parse(t):{};}
await api('/sys/auth/approle',{type:'approle'});
await api('/sys/policies/acl/fixture',{policy:'path "secret/*" { capabilities = ["create", "read", "update", "delete", "list"] }'});
await api('/auth/approle/role/fixture',{token_policies:['fixture']});
const role=(await api('/auth/approle/role/fixture/role-id')).data.role_id;
const secret=(await api('/auth/approle/role/fixture/secret-id',{})).data.secret_id;
await api('/secret/data/xolotl/oidc',{data:{client_secret:'synthetic-oidc-secret'}});
await api('/secret/data/xolotl/admin',{data:{token:'synthetic-admin-token'}});
writeFileSync(new URL('./config.toml', import.meta.url),`[server]\nlisten = "0.0.0.0:34317"\n[database]\npath = "/data/xolotl.db"\n[vault]\naddress = "http://127.0.0.1:34445"\nmount = "secret"\nprefix = "xolotl"\n[vault.approle]\nrole_id = "${role}"\nsecret_id = "${secret}"\n[ui.oidc]\nissuer = "http://localhost:34444"\nclient_id = "xolotl"\nredirect_url = "http://localhost:34317/admin/session/callback"\nadmin_group = "admins"\n[catalog]\nmetadata_url = ""\n[catalog.static_models]\nfixture = ["fixture-model"]\n[telemetry]\nlog_filter = "warn"\n`);
console.log('Synthetic AppRole, OIDC and admin bootstrap seeded; config written.');
