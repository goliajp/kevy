/* MPS2-AN386 (Cortex-M4): code at 0, SRAM at 0x20000000. */
MEMORY
{
  FLASH : ORIGIN = 0x00000000, LENGTH = 4M
  RAM   : ORIGIN = 0x20000000, LENGTH = 4M
}
ENTRY(reset);
SECTIONS
{
  .vector_table ORIGIN(FLASH) :
  {
    LONG(0x20400000);                    /* slot 0: initial stack pointer (top of SRAM) */
    KEEP(*(.vector_table));              /* slot 1: reset */
    KEEP(*(.vector_table.exceptions));   /* slots 2-15: NMI .. SysTick */
  } > FLASH
  .text   : { *(.text .text.*); } > FLASH
  .rodata : { *(.rodata .rodata.*); } > FLASH

  .bss (NOLOAD) : ALIGN(4)
  {
    __sbss = .;
    *(.bss .bss.*);
    *(COMMON);
    . = ALIGN(4);
    __ebss = .;
  } > RAM

  /DISCARD/ : { *(.ARM.exidx .ARM.exidx.*); }
}
