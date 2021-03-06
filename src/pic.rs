/*
 * PIC emulation
 */

use vm;

use std::rc::Rc;
use std::cell::RefCell;

const PIC_MASTER_CMD: u16 = 0x20;
const PIC_MASTER_DATA: u16 = 0x21;
const PIC_SLAVE_CMD: u16 = 0xA0;
const PIC_SLAVE_DATA: u16 = 0xA1;

const ICW1_INIT: u8 = 0x10;
const ICW1_ICW4: u8 = 0x01;
const ICW4_8086: u8 = 0x01;

const PIC_READ_IRR: u8 = 0x0A;
const PIC_READ_ISR: u8 = 0x0B;
const PIC_EOI: u8 = 0x20;

/**
 * i8259 PIC chip
 */
struct I8259A
{
    irr: u8,    // IRR register
    isr: u8,    // ISR register
    imr: u8,    // IRQ mask
    offset: u8, // Interrupt vector base
    icw3: u8,   // ICW3 value during initialization (cascade IRQ)
    next_icw: usize,    // During init, next ICW word expected during init
    cmd_latch: u8,      // Latched value to be read next time from command port
}

impl I8259A 
{
    fn default() -> I8259A {
        I8259A { 
            irr: 0,
            isr: 0,
            imr: 0,
            offset: 0,
            icw3: 0,
            next_icw: 0,
            cmd_latch: 0,
        }
    }

    fn is_initialized(&self) -> bool {
        self.next_icw == 1
    }

    fn slave_irq(&self) -> u8 {
        self.icw3
    }

    /* Assert an IRQ line */
    fn assert_irq(&mut self, irq: u8) {
        assert!(irq < 8);

        if !self.is_initialized() {
            return;
        }

        let mask = 1u8 << irq;
        if (self.imr & mask) != 0 {
            return;
        }

        /* We only update IRR here because we're not sure when
         * interrupt event is going to be injected in guest.
         * That what ack is for. */
        self.irr |= mask;

        /* Notify VM state we need to inject this vector */
        vm::raise_external_interrupt(irq + self.offset);
    }

    /* Acknowledge interrupt delivery to guest */
    fn ack(&mut self, vec: u8) {
        assert!(vec >= self.offset);
        let irq = vec - self.offset;

        /* Acked bit should be in IRR */
        assert!(0 != (self.irr & (1_u8 << irq)));

        /* Move IRR bit to ISR */
        self.isr |= 1_u8 << irq;
        self.irr &= !(1_u8 << irq);
    }

    /* Write to command port */
    fn write_command(&mut self, cmd: u8) {
        if cmd & ICW1_INIT != 0 {
            /* Start initialization
             * We support only ICW1 + ICW4 (and ICW4 should set 8086 tyoe) */
            assert!(cmd & !(ICW1_INIT | ICW1_ICW4) == 0);
            self.next_icw = 2;
            self.imr = 0;

            /* What happens to raised but not yet injected guest interrupts at this point?
             * Intel spec is not entirely clear on that regard, however continuing to deliver
             * those interrupts can be bad since guest might now change IRQ offsets
             * and, accordingly, it's IDT.
             *
             * We can't yet reinject any of those interrupts since we don't know new offsets, so we
             * do that in following steps:
             * 1. Cancel any interrupts that might have been raised according to our current IRR.
             *    Don't touch IRR value.
             * 2. Upon completed init reinject all pending IRR interrupts with updated offsets.
             */
            if self.irr != 0 {
                vm::cancel_all_external_interrupts();
            }

            /* Also, what if an interrupt was delivered (ISR != 0) but not EOI-ed by the guest?
             * Strictly speaking this is a guest bug.
             * It might deliver a racy EOI after init so let's keep ISR hanging as well */
        } else if cmd == PIC_READ_IRR {
            self.cmd_latch = self.irr;
        } else if cmd == PIC_READ_ISR {
            self.cmd_latch = self.isr;
        } else if cmd == PIC_EOI {
            if self.isr != 0 {
                /* TODO: abstract away (and optimize) bsf */
                let mut isr = self.isr;
                let mut pos = 0;
                while (isr & 0x1) == 0 {
                    pos += 1;
                    isr >>= 1;
                }

                self.isr = self.isr & !(1 << pos);
            }
        } else {
            debug!("Unsupported PIC command {:x}", cmd);
        }
    }

    /* Read from command port */
    fn read_command(&mut self) -> u8 {
        return self.cmd_latch;
    }

    /* Read from data port */
    fn read_data(&mut self) -> u8 {
        self.imr
    }

    /* Write to data port */
    fn write_data(&mut self, data: u8) {
        match self.next_icw {
            2 => {
                self.offset = data;
                self.next_icw = 3;
            },

            3 => {
                self.icw3 = data;
                self.next_icw = 4;
            },

            4 => {
                assert!(data == ICW4_8086); /* Just check that ICW4 is the only one we support */
                self.next_icw = 1; /* Init sequence complete */

                /* Re-inject pre-reset pending interrupts from IRR.
                 * See comments in write_command ICW1 */
                for i in 0..8 {
                    if (self.irr & (1_u8 << i)) != 0 {
                        vm::raise_external_interrupt(i + self.offset);
                    }
                }
            },

            _ => {
                self.imr = data; /* Outside init sequence all writes go to IMR by default */
            }
        }
    }
}

///////////////////////////////////////////////////////////////////////////////

/**
 * Cascade PIC setup
 */
struct PIC
{
    master: I8259A,
    slave: I8259A,
}

impl PIC
{
    fn new() -> PIC {
        PIC {
            master: I8259A::default(),
            slave: I8259A::default(),
        }
    }

    fn assert_irq(&mut self, irq: u8) {
        assert!(irq <= 15);
        if irq < 8 {
            self.master.assert_irq(irq);
        } else {
            let slave_irq = self.master.slave_irq();
            self.master.assert_irq(slave_irq);
            self.slave.assert_irq(irq - 8);
        }
    }

    fn ack(&mut self, vec: u8) {
        if vec >= self.slave.offset {
            self.slave.ack(vec);
        } else {
            self.master.ack(vec);
        }
    }
}

///////////////////////////////////////////////////////////////////////////////

#[cfg(test)]
mod i8259a_test 
{
    use super::I8259A;

    fn init_common(offset: u8, mask: u8, cascade: u8) -> I8259A {
        let mut dev = I8259A::default();
        assert!(!dev.is_initialized());

        dev.write_command(super::ICW1_INIT | super::ICW1_ICW4);
        assert!(!dev.is_initialized());

        dev.write_data(offset);
        assert!(!dev.is_initialized());

        dev.write_data(cascade);
        assert!(!dev.is_initialized());

        dev.write_data(super::ICW4_8086);
        assert!(dev.is_initialized());

        dev.write_data(mask);
        assert!(dev.read_data() == mask);

        return dev;
    }

    /* Init with ICW4 */
    #[test] fn init() {
        let dev = init_common(0x08, 0xAB, 0x02);
        assert!(dev.is_initialized());
    }
}

///////////////////////////////////////////////////////////////////////////////

struct PICDev
{
    pic: RefCell<PIC>,
}

impl vm::io_handler for PICDev
{
    fn io_read(&self, port: u16, size: u8) -> vm::IoOperandType
    {
        assert!(size == 1);

        let mut dev = self.pic.borrow_mut();
        vm::IoOperandType::byte(
            match port {
                PIC_MASTER_DATA => dev.master.read_data(),
                PIC_MASTER_CMD => dev.master.read_command(),

                PIC_SLAVE_DATA => dev.slave.read_data(),
                PIC_SLAVE_CMD => dev.slave.read_command(),

                _ => 0,
            }
        )
    }

    fn io_write(&self, port: u16, data: vm::IoOperandType)
    {
        let mut dev = self.pic.borrow_mut();
        let data8 = data.unwrap_byte();

        match port {
            PIC_MASTER_DATA => dev.master.write_data(data8),
            PIC_MASTER_CMD => dev.master.write_command(data8),

            PIC_SLAVE_DATA => dev.slave.write_data(data8),
            PIC_SLAVE_CMD => dev.slave.write_command(data8),

            _ => panic!(),
        }
    }
}

impl vm::interrupt_controller for PICDev
{
    fn assert_irq(&self, irq: u8)
    {
        let mut dev = self.pic.borrow_mut();
        dev.assert_irq(irq)
    }

    fn ack(&self, vec: u8)
    {
        let mut dev = self.pic.borrow_mut();
        dev.ack(vec)
    }
}

pub fn init()
{
	let dev = Rc::new(PICDev {
        pic: RefCell::new(PIC::new()),
    });

    vm::register_interrupt_controller(dev.clone());

    vm::register_io_region(dev.clone(), PIC_MASTER_CMD, 1);
    vm::register_io_region(dev.clone(), PIC_MASTER_DATA, 1);
    vm::register_io_region(dev.clone(), PIC_SLAVE_CMD, 1);
    vm::register_io_region(dev.clone(), PIC_SLAVE_DATA, 1);
}

