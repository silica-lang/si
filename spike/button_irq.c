/* Spike 2: GPIOTE falling-edge → NVIC IRQ → handler toggles LED.
 *
 * De-risks the Stage-D path of the on-metal scope (the `on button.falling`
 * binding → GPIOTE channel + vector-table entry + IRQ handler).  Throwaway,
 * hand-written; mirrors what the metal backend will generate.
 *
 * Button1 = P0.11 (input, pull-up).  LED1 = P0.13 (output).  A GPIOTE channel
 * watches P0.11 for a HiToLo (falling) edge and raises GPIOTE_IRQn (6); the
 * handler clears the event and toggles the LED.
 */
#include <stdint.h>

#define P0_BASE      0x50000000UL
#define GPIO_OUT     (*(volatile uint32_t *)(P0_BASE + 0x504))
#define GPIO_OUTSET  (*(volatile uint32_t *)(P0_BASE + 0x508))
#define GPIO_DIRSET  (*(volatile uint32_t *)(P0_BASE + 0x518))
#define GPIO_PIN_CNF(n) (*(volatile uint32_t *)(P0_BASE + 0x700 + 4UL * (n)))

#define GPIOTE_BASE      0x40006000UL
#define GPIOTE_EVENTS_IN0 (*(volatile uint32_t *)(GPIOTE_BASE + 0x100))
#define GPIOTE_INTENSET   (*(volatile uint32_t *)(GPIOTE_BASE + 0x304))
#define GPIOTE_CONFIG0    (*(volatile uint32_t *)(GPIOTE_BASE + 0x510))

#define NVIC_ISER0   (*(volatile uint32_t *)0xE000E100UL)

#define LED_PIN 13
#define BTN_PIN 11
#define GPIOTE_IRQN 6

extern uint32_t _estack;
void Reset_Handler(void);
void GPIOTE_IRQHandler(void);
static void default_handler(void) { for (;;) {} }

/* Vector table: SP, reset, the 14 system exceptions, then external IRQs.
 * GPIOTE is external IRQ 6 → index 16 + 6 = 22. */
__attribute__((section(".vectors"), used))
const void *const vectors[] = {
    (void *)&_estack,         /* 0  initial SP            */
    (void *)Reset_Handler,    /* 1  reset                 */
    (void *)default_handler,  /* 2  NMI                   */
    (void *)default_handler,  /* 3  HardFault             */
    0, 0, 0, 0, 0, 0, 0,      /* 4..10 reserved/unused    */
    (void *)default_handler,  /* 11 SVCall                */
    0, 0,                     /* 12..13                   */
    (void *)default_handler,  /* 14 PendSV                */
    (void *)default_handler,  /* 15 SysTick               */
    /* external IRQs 0..6 */
    (void *)default_handler,  /* 16 IRQ0  POWER_CLOCK     */
    (void *)default_handler,  /* 17 IRQ1                  */
    (void *)default_handler,  /* 18 IRQ2                  */
    (void *)default_handler,  /* 19 IRQ3                  */
    (void *)default_handler,  /* 20 IRQ4                  */
    (void *)default_handler,  /* 21 IRQ5                  */
    (void *)GPIOTE_IRQHandler,/* 22 IRQ6  GPIOTE          */
};

void GPIOTE_IRQHandler(void) {
    GPIOTE_EVENTS_IN0 = 0;          /* clear the event */
    GPIO_OUT ^= (1UL << LED_PIN);   /* toggle the LED  */
}

void Reset_Handler(void) {
    GPIO_DIRSET = (1UL << LED_PIN);          /* LED output */
    GPIO_OUTSET = (1UL << LED_PIN);          /* LED off (active-low) initially */
    GPIO_PIN_CNF(BTN_PIN) = (3UL << 2);      /* input, pull-up */

    /* GPIOTE ch0: event mode, pin 11, port 0, HiToLo (falling) polarity. */
    GPIOTE_CONFIG0 = 1UL | (BTN_PIN << 8) | (2UL << 16);
    GPIOTE_INTENSET = 1UL;                    /* enable IN[0] event interrupt */
    NVIC_ISER0 = (1UL << GPIOTE_IRQN);        /* enable GPIOTE IRQ in NVIC */

    __asm__ volatile("cpsie i");              /* enable interrupts */
    for (;;) {
        __asm__ volatile("wfi");
    }
}
