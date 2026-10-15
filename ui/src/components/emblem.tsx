import { useId, type ComponentProps } from "react";

/**
 * The xolotl emblem: a canine in a portal. The ink follows `currentColor`, so it
 * sits on the dark sidebar and the light card alike; the portal is the rust accent.
 * `brand/xolotl-emblem.svg` is the master; keep the geometry here the same.
 */
export function Emblem({ title = "Xolotl", ...props }: ComponentProps<"svg"> & { title?: string }) {
  const id = useId();
  const lines = `${id}-lines`;
  const shapes = `${id}-shapes`;
  const pillar = `${id}-pillar`;
  const knockout = `${id}-knockout`;
  return (
    <svg xmlns="http://www.w3.org/2000/svg" viewBox="306 202 670 835" role="img" aria-label={title} {...props}>
      <defs>
        <g id={lines}>
          <path d="M 614,300 C 545,391 424,530 528,640 C 418,688 378,845 385,952" />
          <path d="M 614,300 V 576 H 650 C 733,589 743,647 800,691 C 851,711 902,737 955,752" />
          <path d="M 657,860 C 640,850 630,832 650,820 C 752,778 864,926 915,812" />
        </g>
        <g id={shapes}>
          <path d="M 649,397 L 683,351 A 15 15 0 0 1 713,351 V 558 H 683 V 402 Z" />
          <path d="M 598,652 Q 600,637 640,637 L 668,637 L 738,710 Q 700,716 665,714 Q 610,700 598,652 Z M 632,662 Q 670,655 702,686 Q 668,698 632,662 Z" />
          <path d="M 890,742 L 940,740 C 962,740 972,748 971,762 C 970,786 955,806 935,822 C 922,832 910,838 895,840 Z M 905,752 C 925,758 938,770 936,783 C 926,786 912,782 907,770 Z" />
          <path d="M 504,745 C 515,869 676,897 678,1033 L 713,1033 C 717,875 519,847 532,700 Q 504,705 504,745 Z" />
          <path d="M 469,780 C 408,911 512,949 581,1033 L 606,1033 C 564,933 447,917 471,780 Z" />
        </g>
        <g id={pillar}>
          <rect x="310" y="469" width="29" height="483" />
          <rect x="360" y="469" width="29" height="483" />
        </g>
        <mask id={knockout} maskUnits="userSpaceOnUse" x="0" y="0" width="1254" height="1254">
          <rect width="1254" height="1254" fill="#fff" />
          <use href={`#${lines}`} fill="none" stroke="#000" strokeWidth="92" strokeLinejoin="round" strokeLinecap="round" />
          <use href={`#${shapes}`} fill="#000" stroke="#000" strokeWidth="60" strokeLinejoin="round" />
          <rect x="611" y="190" width="32" height="70" fill="#000" />
          <circle cx="349" cy="582" r="45" fill="#000" />
          <circle cx="905" cy="582" r="45" fill="#000" />
        </mask>
      </defs>
      <g fill="var(--primary)">
        <g mask={`url(#${knockout})`}>
          <path d="M 321.6,447 A 314 314 0 0 1 932.4,447 L 889,447 A 272 272 0 0 0 365,447 Z" />
          <use href={`#${pillar}`} />
          <use href={`#${pillar}`} transform="matrix(-1 0 0 1 1254 0)" />
        </g>
        <circle cx="349" cy="582" r="25" />
        <circle cx="905" cy="582" r="25" />
      </g>
      <g fill="currentColor" stroke="currentColor">
        <use href={`#${lines}`} fill="none" strokeWidth="32" strokeLinejoin="round" strokeLinecap="round" />
        <use href={`#${shapes}`} stroke="none" fillRule="evenodd" />
      </g>
    </svg>
  );
}
