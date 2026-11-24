import sqlite3,time
c=sqlite3.connect('/data/xolotl.db')
now=int(time.time())
rows=[(now-3600,'team/editor','gateway-a','acct-a','model-a',200,100,25,50,240),
(now-7200,'team/editor','gateway-a','acct-a','model-a',500,80,20,30,140),
(now-30*3600,None,'gateway-b',None,'model-b',200,200,50,100,340),
(now-40*3600,'old-client','gateway-a','acct-a','model-a',200,400,80,200,400)]
c.executemany('INSERT INTO request_log(ts,client_id,provider,account_id,model,status,input_tokens,output_tokens,cached_tokens,latency_ms,endpoint) VALUES(?,?,?,?,?,?,?,?,?,?,"/v1/chat/completions")',rows)
c.commit()
print('Seeded four synthetic request rows, including 30h and 40h history.')
