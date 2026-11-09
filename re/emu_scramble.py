import sys, struct, json
from unicorn import *
from unicorn.x86_const import *
O="/Work/3rd/rea/orico/raid-mgr/Win/ORICO_RAID_Manager_Win_561_v0.00.00.21/Application/raidapi.o"
d=open(O,'rb').read()
text=d[0xb4:0xb4+0xc9d0]
# check no reloc inside Scrambling (0xf0..0x11f0) -- verified by objdump -r earlier (none listed)
def scramble(data):
    mu=Uc(UC_ARCH_X86,UC_MODE_32)
    CODE=0x10000000; STK=0x20000000; BUF=0x30000000
    mu.mem_map(CODE,0x20000); mu.mem_write(CODE,text)
    mu.mem_map(STK,0x10000); mu.mem_map(BUF,0x10000)
    mu.mem_write(BUF,data)
    ret=CODE+0x19000
    mu.mem_write(ret, b'\xf4')
    sp=STK+0x8000
    mu.mem_write(sp, struct.pack('<I',ret))
    mu.reg_write(UC_X86_REG_ESP,sp)
    mu.reg_write(UC_X86_REG_EAX,BUF)
    mu.reg_write(UC_X86_REG_EDX,BUF+0x4000)
    mu.reg_write(UC_X86_REG_ECX,len(data))
    try:
        mu.emu_start(CODE+0xf0, ret)
    except UcError as e:
        pass
    return bytes(mu.mem_read(BUF+0x4000,len(data)))
if __name__=="__main__":
    import os
    zero=scramble(bytes(512))
    print(zero[:32].hex())
    rnd=os.urandom(512)
    out=scramble(rnd)
    ks=bytes(a^b for a,b in zip(out,rnd))
    print("keystream matches zero-input:", ks==zero)
    json.dump({"keystream_hex":zero.hex()}, open(sys.argv[1],'w'))
