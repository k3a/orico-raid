import re,json,sys
src=open(sys.argv[1]).read().split('\n')
# Scrambling body lines 81..285 (1-indexed) of raidapi.c
body='\n'.join(src[80:285])
# P: local_9c[i] = local_3c[k] ^ local_7c[i]
P={}
for m in re.finditer(r'local_9c\[(0x[0-9a-f]+|\d+)\] = local_3c\[(0x[0-9a-f]+|\d+)\] \^ local_7c\[(0x[0-9a-f]+|\d+)\];',body):
    i,k,j=[int(x,0) for x in m.groups()]
    assert i==j
    P[i]=k
assert len(P)==32
# M: statements assigning bVarN and local_5c[j]
start=body.index('bVar2 = local_3c[0x1f]')
stmts=body[start:]
stmts=stmts[:stmts.index('uVar9 = 0;\n    local_5c[2]')] + 'local_5c[2]'+ stmts[stmts.index('local_5c[2]')+11:] if False else stmts
# flatten into python
code=[]
for s in re.split(r';\s*\n',stmts):
    s=s.strip()
    if not s: continue
    s=re.sub(r'\s+',' ',s)
    m=re.match(r'^(bVar\d+|local_5c\[(?:0x[0-9a-f]+|\d+)\]) = (.*)$',s)
    if not m: continue
    lhs,rhs=m.groups()
    rhs=rhs.replace('local_3c[','K[')
    lhs=lhs.replace('local_5c[','M[')
    code.append(f"{lhs} = {rhs}")
K=[1<<i for i in range(32)]
M={}
env={'K':K,'M':M}
for c in code:
    # python: '^' on ints is xor; indexes like 0x1f fine
    exec(c,env)
M=env['M']
assert sorted(M.keys())==list(range(32)), sorted(M.keys())
json.dump({'perm':[P[i] for i in range(32)],'next':[M[i] for i in range(32)]},open(sys.argv[2],'w'))
# verify vs keystream
ks=bytes.fromhex(json.load(open(sys.argv[3]))['keystream_hex'])
key=0x52325032
out=b''
for w in range(128):
    o=0
    for i in range(32):
        o|=((key>>P[i])&1)<<i
    out+=o.to_bytes(4,'little')
    nk=0
    for j in range(32):
        nk|=(bin(key&M[j]).count('1')&1)<<j
    key=nk
print("match", out==ks)
print([hex(M[j]) for j in range(32)])
print(P)
