"""Run the ORIGINAL vendor machine code (raidapi.o) in an emulator against a fake disk
and dump the exact sectors it writes. Used as test oracle for the Rust protocol layer."""
import struct, sys, json
from unicorn import *
from unicorn.x86_const import *
O="/Work/3rd/rea/orico/raid-mgr/Win/ORICO_RAID_Manager_Win_561_v0.00.00.21/Application/raidapi.o"
d=open(O,'rb').read()
machine,nsec,ts,symptr,nsyms,optsz,flags=struct.unpack_from('<HHIIIHH',d,0)
secs=[]
for i in range(nsec):
    o=20+optsz+40*i
    name,vsize,vaddr,rawsz,rawptr,relptr,lnptr,nrel,nln,fl=struct.unpack_from('<8sIIIIIIHHI',d,o)
    secs.append(dict(name=name.rstrip(b'\0').decode(),vsize=vsize,rawsz=rawsz,rawptr=rawptr,relptr=relptr,nrel=nrel,flags=fl))
strtab=symptr+nsyms*18
def sname(raw):
    if raw[:4]==b'\0\0\0\0':
        off=struct.unpack('<I',raw[4:])[0]
        e=d.index(b'\0',strtab+off); return d[strtab+off:e].decode()
    return raw.rstrip(b'\0').decode()
syms=[];i=0
while i<nsyms:
    o=symptr+18*i
    raw=d[o:o+8]; val,sec,typ,cls,naux=struct.unpack_from('<IhHBB',d,o+8)
    syms.append(dict(name=sname(raw),value=val,sec=sec,cls=cls,idx=i))
    for _ in range(naux): syms.append(None)
    i+=1+naux
# memory layout
BASE=0x10000000
lay={}; cur=BASE
for k,s in enumerate(secs):
    size=max(s['rawsz'],s['vsize']) if s['rawsz'] else 0
    # bss has rawsz==0 -> size from header vsize? for COFF obj, bss size lives in rawsz-field (flags uninit)
    size=s['rawsz'] if s['rawsz'] else s['vsize']
    lay[k+1]=cur; cur+=(size+0xfff)&~0xfff
bss_idx=[k+1 for k,s in enumerate(secs) if s['name']=='.bss'][0]
IMP=0x40000000
STK=0x50000000; HEAP=0x60000000
mu=Uc(UC_ARCH_X86,UC_MODE_32)
mu.mem_map(BASE,cur-BASE+0x100000)
for k,s in enumerate(secs):
    if s['rawptr'] and not (s['flags']&0x80):
        mu.mem_write(lay[k+1],d[s['rawptr']:s['rawptr']+s['rawsz']])
mu.mem_map(IMP,0x10000); mu.mem_map(STK,0x100000); mu.mem_map(HEAP,0x400000)
ext={}; extn=[0]
def ext_addr(name):
    if name not in ext:
        ext[name]=IMP+0x100+extn[0]*16; extn[0]+=1
        mu.mem_write(ext[name],b'\xc3')
    return ext[name]
iat={}
def iat_addr(name):
    if name not in iat:
        a=IMP+0x8000+len(iat)*4
        mu.mem_write(a,struct.pack('<I',ext_addr(name[len('__imp_'):] if name.startswith('__imp_') else name)))
        iat[name]=a
    return iat[name]
def symaddr(s):
    if s['sec']>0: return lay[s['sec']]+s['value']
    if s['name'].startswith('__imp_'): return iat_addr(s['name'])
    if s['sec']==0 and s['value']>0: # common symbol
        raise Exception('common '+s['name'])
    return ext_addr(s['name'])
for k,s in enumerate(secs):
    for r in range(s['nrel']):
        va,si,ty=struct.unpack_from('<IIH',d,s['relptr']+10*r)
        sym=syms[si]; site=lay[k+1]+va
        cur_=struct.unpack('<I',bytes(mu.mem_read(site,4)))[0]
        a=symaddr(sym)
        if ty==6: v=(a+cur_)&0xffffffff
        elif ty==0x14: v=(a+cur_-(site+4))&0xffffffff
        else: raise Exception('reloc %x'%ty)
        mu.mem_write(site,struct.pack('<I',v))
def S(name):
    for s in syms:
        if s and s['name']==name: return symaddr(s)
    raise KeyError(name)
TEXT=lay[1]
# ---- fake disk / device model
class Dev: pass
dev=Dev(); dev.pos=0; dev.writes=[]; dev.heap=HEAP; dev.sector={}; dev.pat=lambda i:(i*13+5)&0xff
def arg(n): # cdecl/stdcall: arg n (0-based) at esp+4+4n
    return struct.unpack('<I',bytes(mu.mem_read(mu.reg_read(UC_X86_REG_ESP)+4+4*n,4)))[0]
def ret(v,pop=0):
    esp=mu.reg_read(UC_X86_REG_ESP)
    r=struct.unpack('<I',bytes(mu.mem_read(esp,4)))[0]
    mu.reg_write(UC_X86_REG_EAX,v&0xffffffff); mu.reg_write(UC_X86_REG_ESP,esp+4+pop); mu.reg_write(UC_X86_REG_EIP,r)
names={v:k for k,v in ext.items()}
def hook(uc,addr,size,_):
    n=names.get(addr)
    if n is None: return
    if n=='_memset':
        p,v,c=arg(0),arg(1),arg(2); mu.mem_write(p,bytes([v&0xff])*c); ret(p)
    elif n=='_memcpy':
        p,s,c=arg(0),arg(1),arg(2); mu.mem_write(p,bytes(mu.mem_read(s,c))); ret(p)
    elif n=='_malloc':
        c=arg(0); a=dev.heap; dev.heap+=(c+15)&~15; ret(a)
    elif n=='_free': ret(0)
    elif n=='_Sleep@4': ret(0,4)
    elif n=='_SetFilePointer@16':
        lo=arg(1); hip=arg(2); hi=struct.unpack('<I',bytes(mu.mem_read(hip,4)))[0] if hip else 0
        dev.pos=lo|(hi<<32); ret(lo,16)
    elif n=='_WriteFile@20':
        buf,cnt,wp=arg(1),arg(2),arg(3)
        data=bytes(mu.mem_read(buf,cnt)); dev.writes.append((dev.pos,data)); dev.sector[dev.pos]=data
        mu.mem_write(wp,struct.pack('<I',cnt)); ret(1,20)
    elif n=='_ReadFile@20':
        buf,cnt,rp=arg(1),arg(2),arg(3)
        data=dev.respond(dev.pos,cnt); mu.mem_write(buf,data); mu.mem_write(rp,struct.pack('<I',cnt)); ret(1,20)
    else:
        raise Exception('unhandled import '+n)
mu.hook_add(UC_HOOK_CODE,hook,begin=IMP,end=IMP+0x8000)
sys.path.insert(0,'.')
exec(open(sys.argv[0].replace('oracle.py','ks.py')).read()) if False else None
# keystream from emulation of vendor Scrambling itself
def vendor_scramble(data):
    mu.mem_write(HEAP+0x300000,data)
    return call(S('__Z11_ScramblingPcS_m'),regs=(HEAP+0x300000,HEAP+0x310000,len(data)),stack=[],outbuf=(HEAP+0x310000,len(data)))
def call(addr,regs=(0,0,0),stack=(),outbuf=None):
    sp=STK+0x80000
    mu.reg_write(UC_X86_REG_EAX,regs[0]); mu.reg_write(UC_X86_REG_EDX,regs[1]); mu.reg_write(UC_X86_REG_ECX,regs[2])
    RET=TEXT+0xf000-16 # in padding? use stub
    RET=IMP+0xf000; mu.mem_write(RET,b'\xf4')
    words=[RET]+list(stack)
    esp=sp-4*len(words)
    mu.mem_write(esp,struct.pack('<%dI'%len(words),*words))
    mu.reg_write(UC_X86_REG_ESP,esp)
    try: mu.emu_start(addr,RET,count=5000000)
    except UcError as e: pass
    eax=mu.reg_read(UC_X86_REG_EAX)
    return bytes(mu.mem_read(*outbuf)) if outbuf else eax
# init CRC table: normally done by InitAllController; do it ourselves like vendor code
def init_crc():
    t=[]
    for i in range(256):
        c=i<<24
        for _ in range(8): c=((c<<1)^0x4c11db7)&0xffffffff if c&0x80000000 else (c<<1)&0xffffffff
        t.append(c)
    base=lay[bss_idx]+0x258
    mu.mem_write(base,struct.pack('<256I',*t))
init_crc()
KS=vendor_scramble(bytes(512))
def sc(b): return bytes(x^y for x,y in zip(b,KS))
SEQ=777; SECT=0x27
def respond(pos,cnt):
    # reply to last written command frame
    w=[x for x in dev.writes if x[0]==pos][-1][1]
    f=bytearray(sc(w))   # unscramble what host wrote
    seq=struct.unpack('<I',f[4:8])[0]
    out=bytearray(512)
    out[0:4]=struct.pack('<I',0x197b0562); out[4:8]=struct.pack('<I',seq); out[0]=0x62
    out[9]=f[9]; out[10]=f[10]; out[11]=0
    for i in range(0xc,0x1fa): out[i]=dev.pat(i)
    return sc(bytes(out))
dev.respond=respond
results={}
def run(name,addr,regs,stack,outaddr,outlen,zero=True):
    dev.writes=[]
    if zero: mu.mem_write(outaddr,bytes(outlen))
    r=call(addr,regs=regs,stack=stack)
    wr=[w for w in dev.writes]
    out=bytes(mu.mem_read(outaddr,outlen))
    results[name]=dict(ret=r&0xff,writes=[(p,sc(b).hex()) for p,b in wr],out=out.hex())
H=0x1234; OUT=HEAP+0x200000; FIS=HEAP+0x250000
off=SECT*512
run('chip_info',S('__Z12_GetChipInfoPvimP21_JMB32X_GET_CHIP_INFO'),(H,SECT,SEQ),[OUT],OUT,0x1f4)
run('all_sata',S('__Z15_GetAllSataInfoPvimP21_JMB32X_SATADISK_INFO'),(H,SECT,SEQ+1),[OUT],OUT,0x190)
run('raid_port3',S('__Z16_GetRaidPortInfoPvihmP22_JMB32X_RAID_PORT_INFO'),(H,SECT,3),[SEQ+2,OUT],OUT,0x1a4)
run('sata_port1',S('__Z16_GetSataPortInfoPvihmP22_JMB32X_SATA_PORT_INFO'),(H,SECT,1),[SEQ+3,OUT],OUT,0x184)
fis=bytearray(0x1e0); fis[2]=0xd0; fis[8]=0x4f; fis[10]=0xc2; fis[12]=0xa0; fis[14]=0xb0
mu.mem_write(FIS,bytes(fis))
run('smart_fis',S('__Z11_SentFisCmdPvimP19_JMB32X_COMMAND_FIShhhm'),(H,SECT,SEQ+4),[FIS,2,2,0,0xe0],FIS,0x1e0,zero=False)
# handshake (plain frames)
dev.writes=[]
hs=[]
for m in (2,3,0,1):
    dev.writes=[]
    call(S('__Z13_HandShakeCmdPv14_LARGE_INTEGERh'),regs=(H,SECT*512,0),stack=[m])
    hs.append(dev.writes[0][1].hex())
results['handshake']=hs
results['keystream']=KS.hex()
json.dump(results,open(sys.argv[1],'w'),indent=1)
for k,v in results.items():
    if k in('handshake','keystream'): continue
    print(k,v['ret'],[(p,w[:60]) for p,w in v['writes']])
