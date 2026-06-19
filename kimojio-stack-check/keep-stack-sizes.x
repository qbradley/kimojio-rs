SECTIONS
{
  .stack_sizes (INFO) :
  {
    KEEP(*(.stack_sizes));
  }
}
INSERT AFTER .strtab;
